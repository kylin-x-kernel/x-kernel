// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::NonNull;
use riscv::register::sie;
use unittest::{assert, assert_eq, def_test};

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};

use super::{NonZeroU32, PlicCompletion, complete_plic_claim};

#[def_test]
fn plic_source_count_requires_one_supported_nonzero_cell() {
    use super::firmware::PlicSources;
    assert!(PlicSources::from_property(None).is_err());
    assert!(PlicSources::from_property(Some(&[])).is_err());
    assert!(PlicSources::from_property(Some(&[0, 0, 1])).is_err());
    assert!(PlicSources::from_property(Some(&[0; 8])).is_err());
    assert!(PlicSources::from_property(Some(&0u32.to_be_bytes())).is_err());
    assert!(PlicSources::from_property(Some(&1024u32.to_be_bytes())).is_err());
    assert!(PlicSources::from_property(Some(&u32::MAX.to_be_bytes())).is_err());
    assert_eq!(
        PlicSources::from_property(Some(&1u32.to_be_bytes()))
            .unwrap()
            .last,
        1
    );
    assert_eq!(
        PlicSources::from_property(Some(&1023u32.to_be_bytes()))
            .unwrap()
            .last,
        1023
    );
}

#[def_test]
fn plic_source_validation_rejects_zero_out_of_range_and_truncation() {
    let sources = super::firmware::PlicSources::from_property(Some(&53u32.to_be_bytes())).unwrap();
    assert!(sources.source(0).is_none());
    assert_eq!(sources.source(1).unwrap().get(), 1);
    assert_eq!(sources.source(53).unwrap().get(), 53);
    assert!(sources.source(54).is_none());
    assert!(sources.source(1023).is_none());
    assert!(sources.source(usize::MAX).is_none());
    assert!(sources.source((1usize << 32) + 1).is_none());
}

#[derive(Debug, PartialEq, Eq)]
enum CompletionAccess {
    Enable(bool),
    Complete,
}

#[def_test]
fn context_preparation_clears_supported_sources_without_completing_claims() {
    let mut model = CompletionModel {
        enabled: u32::MAX,
        priority: 0,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    super::disable_plic_sources(&mut model, 3, super::firmware::PlicSources { last: 7 });
    assert_eq!(model.enabled, !0xfeu32);
    assert_eq!(model.accepted, 0);
    assert!(model.is_claim_active);
    assert_eq!(model.accesses.len(), 7);
    assert!(
        model
            .accesses
            .iter()
            .all(|access| *access == CompletionAccess::Enable(false))
    );
}

struct CompletionModel {
    enabled: u32,
    priority: u32,
    is_claim_active: bool,
    accepted: usize,
    accesses: alloc::vec::Vec<CompletionAccess>,
}

impl CompletionModel {
    fn claim_next(&mut self, source: NonZeroU32, context: usize) -> bool {
        if self.is_claim_active || !self.is_enabled(source, context) {
            return false;
        }
        self.is_claim_active = true;
        true
    }
}

impl super::PlicPriorityRegisters for CompletionModel {
    fn read_priority(&self, source: NonZeroU32) -> u32 {
        core::assert_eq!(source.get(), 5);
        self.priority
    }

    fn write_priority(&mut self, source: NonZeroU32, priority: u32) {
        core::assert_eq!(source.get(), 5);
        self.priority = priority;
    }
}

impl PlicCompletion for CompletionModel {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        core::assert_eq!(context, 3);
        self.enabled & (1 << source.get()) != 0
    }

    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        core::assert_eq!(context, 3);
        let bit = 1 << source.get();
        if enabled {
            self.enabled |= bit;
        } else {
            self.enabled &= !bit;
        }
        self.accesses.push(CompletionAccess::Enable(enabled));
    }

    fn complete(&mut self, context: usize, source: NonZeroU32) {
        // PLIC specification: a disabled target/source silently ignores completion.
        if self.is_enabled(source, context) {
            self.accepted += 1;
            self.is_claim_active = false;
        }
        self.accesses.push(CompletionAccess::Complete);
    }
}

#[def_test]
fn disabled_plic_completion_is_accepted_and_restores_enable_state() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 1 << 7,
        priority: 0,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 1);
    assert!(!model.is_claim_active);
    assert_eq!(model.enabled, 1 << 7);
    assert_eq!(
        model.accesses.as_slice(),
        &[
            CompletionAccess::Enable(true),
            CompletionAccess::Complete,
            CompletionAccess::Enable(false),
        ]
    );
}

#[def_test]
fn plic_management_after_migration_keeps_the_original_target() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 1 << 7,
        priority: 0,
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    let mut target = None;
    super::set_plic_route(&mut model, &mut target, 3, source, true);
    assert_eq!(target, Some(3));
    assert!(model.claim_next(source, 3));
    // The fixture rejects any register access outside the original context 3.
    super::set_plic_route(&mut model, &mut target, 7, source, false);
    assert_eq!(model.enabled, 1 << 7);
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.enabled, 1 << 7);
    assert_eq!(model.accepted, 1);
    super::set_plic_route(&mut model, &mut target, 7, source, true);
    assert_eq!(target, Some(3));
    assert_eq!(model.enabled, (1 << 5) | (1 << 7));
    assert!(model.claim_next(source, 3));
}

#[def_test]
fn plic_disable_before_first_enable_does_not_choose_a_target() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 0,
        priority: 0,
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    let mut target = None;
    super::set_plic_route(&mut model, &mut target, 7, source, false);
    assert_eq!(target, None);
    assert!(model.accesses.is_empty());
    super::set_plic_route(&mut model, &mut target, 3, source, true);
    assert_eq!(target, Some(3));
    assert_eq!(model.enabled, 1 << 5);
}

#[def_test]
fn enabled_plic_completion_does_not_toggle_enable_state() {
    let source = NonZeroU32::new(5).unwrap();
    let enabled = (1 << 5) | (1 << 7);
    let mut model = CompletionModel {
        enabled,
        priority: 0,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 1);
    assert_eq!(model.enabled, enabled);
    assert_eq!(model.accesses.as_slice(), &[CompletionAccess::Complete]);
}

#[def_test]
fn plic_handler_disable_does_not_block_delivery_after_reenable() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: (1 << 5) | (1 << 7),
        priority: 0,
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    assert!(model.claim_next(source, 3));
    model.set_enabled(source, 3, false);
    complete_plic_claim(&mut model, 3, source);
    assert!(!model.claim_next(source, 3));
    model.set_enabled(source, 3, true);
    assert!(model.claim_next(source, 3));
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 2);
    assert!(!model.is_claim_active);
    assert_eq!(model.enabled, (1 << 5) | (1 << 7));
}

#[def_test]
fn plic_mask_preserves_policy_and_completion_does_not_change_it() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 1 << 7,
        priority: 3,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    let mut state = super::PlicSourceState::default();
    state.mask(&mut model, source);
    state.mask(&mut model, source);
    assert_eq!(state.configured_priority, Some(3));
    assert_eq!(model.priority, 0);
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 1);
    assert_eq!(model.enabled, 1 << 7);
    assert_eq!(model.priority, 0);
    assert_eq!(state.configured_priority, Some(3));
    assert!(state.is_masked);
    assert_eq!(state.target_context, None);
    state.unmask(&mut model, source);
    assert_eq!(model.priority, 3);
    assert!(!state.is_masked);
    state.unmask(&mut model, source);
    assert_eq!(model.priority, 3);
}

#[def_test]
fn plic_masked_priority_updates_and_enable_stay_suppressed() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 1 << 7,
        priority: 0,
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    let mut state = super::PlicSourceState::default();
    state.set_priority(&mut model, source, 4);
    assert_eq!(model.priority, 4);
    state.mask(&mut model, source);
    state.set_priority(&mut model, source, 2);
    assert_eq!(model.priority, 0);
    state.unmask(&mut model, source);
    assert_eq!(model.priority, 2);

    state.mask(&mut model, source);
    state.set_enabled(&mut model, 3, source, true);
    assert_eq!(state.configured_priority, Some(6));
    assert_eq!(state.target_context, Some(3));
    assert_eq!(model.priority, 0);
    assert_eq!(model.enabled, (1 << 5) | (1 << 7));
    state.set_enabled(&mut model, 7, source, false);
    assert_eq!(model.enabled, 1 << 7);
    assert_eq!(model.priority, 0);
    state.set_enabled(&mut model, 7, source, true);
    assert_eq!(model.priority, 0);
    state.unmask(&mut model, source);
    assert_eq!(model.priority, 6);
    state.mask(&mut model, source);
    state.set_priority(&mut model, source, 0);
    state.mask(&mut model, source);
    assert_eq!(state.configured_priority, Some(0));
    state.unmask(&mut model, source);
    assert_eq!(model.priority, 0);
    assert!(!state.is_masked);
    state.set_enabled(&mut model, 7, source, true);
    assert_eq!(model.priority, 6);
}

#[def_test]
fn plic_first_mask_preserves_firmware_zero_priority() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 0,
        priority: 0,
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    let mut state = super::PlicSourceState::default();
    state.unmask(&mut model, source);
    assert_eq!(state.configured_priority, None);
    state.mask(&mut model, source);
    state.mask(&mut model, source);
    state.unmask(&mut model, source);
    assert_eq!(state.configured_priority, Some(0));
    assert_eq!(model.priority, 0);
}

#[def_test]
fn plic_flow_selection_requires_supported_binding() {
    use kirq::{
        IrqAffinity, IrqController, IrqDesc, IrqDescError, IrqFlags, IrqFlow, IrqPolarity,
        IrqTrigger,
    };
    let sources = Some(super::firmware::PlicSources { last: 53 });
    let desc = super::plic_irq_desc(5);
    for accepted in [
        desc,
        IrqDesc {
            trigger: IrqTrigger::LevelHigh,
            polarity: IrqPolarity::High,
            ..desc
        },
        desc.with_flags(IrqFlags::SHARED),
        desc.with_flags(IrqFlags::WAKEUP_SOURCE),
        super::plic_irq_desc(53),
    ] {
        let flow = super::select_plic_flow(accepted, sources).unwrap();
        assert!(
            matches!(flow, IrqFlow::Fasteoi(chip) if core::ptr::eq(chip, &super::PLIC_FASTEOI))
        );
    }
    for rejected in [
        IrqDesc::from_hwirq(5).with_domain(super::PLIC_DOMAIN),
        IrqDesc::from_hwirq(5).with_controller(IrqController::Plic),
        desc.with_domain(kirq::GIC_ROOT_DOMAIN),
        desc.with_controller(IrqController::Msi),
        desc.with_flags(IrqFlags::MSI),
        desc.with_flags(IrqFlags::PER_CPU),
        desc.with_flags(IrqFlags::from_bits_retain(1 << 31)),
        desc.with_affinity(IrqAffinity::Cpu(0)),
        desc.with_polarity(IrqPolarity::Low),
        IrqDesc {
            trigger: IrqTrigger::EdgeRising,
            ..desc
        },
        IrqDesc {
            trigger: IrqTrigger::EdgeFalling,
            ..desc
        },
        IrqDesc {
            trigger: IrqTrigger::LevelLow,
            ..desc
        },
        super::plic_irq_desc(0),
        super::plic_irq_desc(54),
        super::plic_irq_desc((1usize << 32) + 5),
    ] {
        assert!(
            matches!(super::select_plic_flow(rejected, sources), Err(IrqDescError::UnsupportedFlow { hwirq }) if hwirq == rejected.hwirq)
        );
    }
    assert!(super::select_plic_flow(desc, None).is_err());
}

struct ForeignBackend;

impl fwnode::FwNodeOps<'static> for ForeignBackend {
    fn source(&self) -> fwnode::FwSource {
        fwnode::FwSource::Static
    }

    fn name(&self, _: fwnode::FwNodeId) -> Option<&'static str> {
        None
    }

    fn parent(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn first_child(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn next_sibling(&self, _: fwnode::FwNodeId) -> Option<fwnode::FwNodeId> {
        None
    }

    fn is_available(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(true)
    }

    fn is_interrupt_controller(&self, _: fwnode::FwNodeId) -> Result<bool, fwnode::FwError> {
        Ok(false)
    }

    fn firmware_id(&self, _: fwnode::FwNodeId, _: usize) -> Option<&'static str> {
        None
    }

    fn property(
        &self,
        _: fwnode::FwNodeId,
        _: &str,
    ) -> Result<Option<fwnode::FwProperty<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn mmio_resource(
        &self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwMmioResource<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn irq_spec(
        &'static self,
        _: fwnode::FwNodeId,
        _: usize,
    ) -> Result<Option<fwnode::FwIrqSpec<'static>>, fwnode::FwError> {
        Ok(None)
    }

    fn reference(
        &'static self,
        _: fwnode::FwNodeId,
        _: &str,
        _: usize,
        _: usize,
    ) -> Result<Option<fwnode::FwReference<'static>>, fwnode::FwError> {
        Ok(None)
    }
}

static FOREIGN_BACKEND: ForeignBackend = ForeignBackend;

#[def_test(serial)]
fn another_plic_instance_domain_is_not_admitted_by_this_driver() {
    use kirq::{IrqController, IrqDescError};

    // Force a dynamically allocated domain even though the boot PLIC already
    // owns the reserved root. Family metadata must not substitute for the
    // instance identifier returned by this driver's own registration.
    let controller = fwnode::FwNode::new(&FOREIGN_BACKEND, fwnode::FwNodeId::new(1));
    let foreign = kirq::register_irq_domain(
        kirq::IrqDomainTarget::New(IrqController::Plic),
        controller,
        &super::PLIC_XLATE,
    )
    .expect("foreign PLIC instance registers its own domain");
    assert_ne!(foreign, super::current_plic_domain());

    let sources = Some(super::firmware::PlicSources { last: 53 });
    assert!(
        super::select_plic_flow(super::plic_irq_desc(5), sources).is_ok(),
        "the initialized instance still accepts its own source"
    );
    for hwirq in [1, 5, 53] {
        assert_eq!(
            super::select_plic_flow(super::plic_irq_desc(hwirq).with_domain(foreign), sources),
            Err(IrqDescError::UnsupportedFlow { hwirq })
        );
    }
}

#[def_test]
fn plic_flow_selection_rejects_unknown_sources_but_preserves_fixed_local_sources() {
    use kirq::{IrqController, IrqDesc, IrqDescError, IrqFlags, IrqFlow, IrqTrigger};
    for desc in [
        IrqDesc::from_hwirq(5),
        IrqDesc::from_virq(kirq::DYNAMIC_VIRQ_BASE),
        IrqDesc::new(0x2f11, IrqTrigger::EdgeRising).with_domain(kirq::GIC_ROOT_DOMAIN),
        IrqDesc::from_hwirq(5).with_controller(IrqController::Gic),
        IrqDesc::from_hwirq(5)
            .with_controller(IrqController::Msi)
            .with_domain(kirq::MSI_DOMAIN)
            .with_flags(IrqFlags::MSI),
        IrqDesc::from_virq(kirq::DYNAMIC_VIRQ_BASE).with_flags(IrqFlags::MSI),
        IrqDesc::from_hwirq(super::S_EXT),
    ] {
        assert_eq!(
            super::select_plic_flow(desc, None),
            Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq })
        );
    }
    let local = IrqDesc::from_hwirq(super::S_TIMER).with_domain(super::PLIC_DOMAIN);
    assert!(super::select_plic_flow(local, None).is_err());
    for cause in [super::S_TIMER, super::S_SOFT] {
        for flags in [IrqFlags::empty(), IrqFlags::PER_CPU] {
            assert!(matches!(
                super::select_plic_flow(IrqDesc::from_hwirq(cause).with_flags(flags), None),
                Ok(IrqFlow::PerCpu)
            ));
        }
    }
}

#[def_test(serial)]
fn source_activation_preserves_global_irq_mask() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let saved = sie::read();
    let mut masked = saved;
    masked.set_ssoft(false);
    masked.set_stimer(false);
    masked.set_sext(false);
    // SAFETY: Global IRQ delivery and migration are excluded; only this hart's
    // source enables change, and the saved state is restored before assertions.
    unsafe { sie::write(masked) };

    super::activate_local_interrupts();
    let activated = sie::read();
    let is_irq_enabled = karch::local_irq_enabled();

    // SAFETY: The guard still excludes delivery and migration on the same hart.
    unsafe { sie::write(saved) };

    let mut expected = saved;
    expected.set_ssoft(true);
    expected.set_stimer(true);
    expected.set_sext(true);
    assert_eq!(activated.bits(), expected.bits());
    assert!(!is_irq_enabled);
}

struct Dtb {
    structure: alloc::vec::Vec<u8>,
    strings: alloc::vec::Vec<u8>,
}

impl Dtb {
    fn new() -> Self {
        Self {
            structure: alloc::vec::Vec::new(),
            strings: alloc::vec::Vec::new(),
        }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4);
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3);
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2);
    }

    fn finish(mut self) -> alloc::vec::Vec<u8> {
        self.word(9);
        let strings_offset = 56 + self.structure.len();
        let mut bytes = alloc::vec::Vec::new();
        for value in [
            0xd00d_feed,
            (strings_offset + self.strings.len()) as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&self.structure);
        bytes.extend_from_slice(&self.strings);
        bytes
    }
}

fn cells(values: &[u32]) -> alloc::vec::Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_be_bytes())
        .collect()
}

fn plic_fixture(ndev: Option<u32>) -> alloc::vec::Vec<u8> {
    plic_fixture_with(ndev, None, true)
}

fn add_test_cpu(blob: &mut Dtb, name: &str, phandle: u32, reg: Option<&[u8]>) {
    blob.begin(name);
    blob.property("device_type", b"cpu\0");
    if let Some(reg) = reg {
        blob.property("reg", reg);
    }
    blob.begin("interrupt-controller");
    blob.property("interrupt-controller", &[]);
    blob.property("#interrupt-cells", &cells(&[1]));
    blob.property("phandle", &cells(&[phandle]));
    blob.end();
    blob.end();
}

fn plic_fixture_with(
    ndev: Option<u32>,
    status: Option<&[u8]>,
    is_controller: bool,
) -> alloc::vec::Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[2]));
    blob.begin("cpus");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[0]));
    add_test_cpu(&mut blob, "cpu@0", 1, Some(&cells(&[0])));
    add_test_cpu(&mut blob, "cpu@1", 2, Some(&cells(&[1])));
    blob.end();
    blob.begin("soc");
    blob.property("compatible", b"simple-bus\0");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[2]));
    blob.property("ranges", &[]);
    blob.begin("interrupt-controller@c000000");
    blob.property("compatible", b"riscv,plic0\0");
    if is_controller {
        blob.property("interrupt-controller", &[]);
    }
    blob.property("#interrupt-cells", &cells(&[1]));
    blob.property("reg", &cells(&[0, 0x0c00_0000, 0, 0x0400_0000]));
    blob.property(
        "interrupts-extended",
        &cells(&[1, 0x0b, 1, 9, 2, 0x0b, 2, 9]),
    );
    if let Some(status) = status {
        blob.property("status", status);
    }
    if let Some(ndev) = ndev {
        blob.property("riscv,ndev", &cells(&[ndev]));
    }
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

fn plic_context_fixture(
    cpus: &[(u32, Option<&[u8]>)],
    entries: &[u32],
    size: u32,
) -> alloc::vec::Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.begin("cpus");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[0]));
    for (index, (phandle, reg)) in cpus.iter().enumerate() {
        let name = alloc::format!("cpu{index}");
        add_test_cpu(&mut blob, &name, *phandle, *reg);
    }
    blob.end();
    blob.begin("soc");
    blob.property("compatible", b"simple-bus\0");
    blob.property("#address-cells", &cells(&[2]));
    blob.property("#size-cells", &cells(&[2]));
    blob.property("ranges", &[]);
    blob.begin("interrupt-controller@c000000");
    blob.property("compatible", b"riscv,plic0\0");
    blob.property("interrupt-controller", &[]);
    blob.property("#interrupt-cells", &cells(&[1]));
    blob.property("reg", &cells(&[0, 0x0c00_0000, 0, size]));
    blob.property("riscv,ndev", &cells(&[53]));
    blob.property("interrupts-extended", &cells(entries));
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

fn plic_region(
    bytes: &[u8],
) -> Result<Option<(memaddr::PhysAddr, usize, u32)>, super::firmware::PlicConfigError> {
    let tree = rs_fdtree::LinuxFdt::new(bytes).unwrap();
    let backend = rs_fdtree::DtBackend::new(tree);
    super::PlicConfig::from_fwnode(backend.root().unwrap()).map(|config| {
        config.map(|config| (config.paddr, config.size, config.sources.last))
    })
}

#[def_test]
fn plic_discovery_uses_translated_firmware_resource() {
    let (paddr, size, sources) = plic_region(&plic_fixture(Some(53))).unwrap().unwrap();
    assert_eq!(paddr.as_usize(), 0x0c00_0000);
    assert_eq!(size, 0x0400_0000);
    assert_eq!(sources, 53);
}

#[def_test]
fn plic_discovery_rejects_malformed_or_missing_description() {
    assert!(plic_region(&plic_fixture(None)).is_err());
    assert!(plic_region(&plic_fixture(Some(1024))).is_err());

    let mut blob = Dtb::new();
    blob.begin("");
    blob.end();
    assert!(plic_region(&blob.finish()).unwrap().is_none());
}

#[def_test]
fn plic_discovery_honors_controller_and_availability_declarations() {
    assert!(
        plic_region(&plic_fixture_with(Some(53), None, false))
            .unwrap()
            .is_none()
    );
    assert!(
        plic_region(&plic_fixture_with(Some(53), Some(b"disabled\0"), true))
            .unwrap()
            .is_none()
    );
    assert!(plic_region(&plic_fixture_with(Some(53), Some(b"\x01"), true)).is_err());
}

fn parse_declarations(
    bytes: &[u8],
) -> Result<Vec<super::context::PlicContextDecl>, super::firmware::PlicConfigError> {
    let tree = rs_fdtree::LinuxFdt::new(bytes).unwrap();
    let backend = rs_fdtree::DtBackend::new(tree);
    let root = backend.root().unwrap();
    let plic = root
        .descendants()
        .find(|node| node.name() == Some("interrupt-controller@c000000"))
        .unwrap();
    super::firmware::parse_contexts(plic, plic.id())
}

fn build_contexts(
    declarations: &[super::context::PlicContextDecl],
    size: usize,
    map_cpu: impl FnMut(u64) -> Option<LogicalCpuId>,
) -> Result<super::context::PlicContexts, super::context::PlicContextError> {
    super::context::PlicContexts::build(declarations, size, map_cpu)
}

fn parsed_contexts(
    bytes: &[u8],
    map_cpu: impl FnMut(u64) -> Option<LogicalCpuId>,
) -> Result<super::context::PlicContexts, super::context::PlicContextError> {
    let declarations = parse_declarations(bytes).unwrap();
    build_contexts(&declarations, 0x0400_0000, map_cpu)
}

#[derive(Debug, PartialEq, Eq)]
enum ContextAccess {
    Enable(bool),
    InitContext(usize),
    Complete(usize),
}

struct ContextModel {
    context: usize,
    enabled: bool,
    threshold: u32,
    accesses: Vec<ContextAccess>,
}

impl ContextModel {
    fn new(context: usize, enabled: bool) -> Self {
        Self {
            context,
            enabled,
            threshold: u32::MAX,
            accesses: Vec::new(),
        }
    }
}

impl PlicCompletion for ContextModel {
    fn is_enabled(&self, _: NonZeroU32, context: usize) -> bool {
        core::assert_eq!(context, self.context);
        self.enabled
    }

    fn set_enabled(&mut self, _: NonZeroU32, context: usize, enabled: bool) {
        core::assert_eq!(context, self.context);
        self.enabled = enabled;
        self.accesses.push(ContextAccess::Enable(enabled));
    }

    fn complete(&mut self, context: usize, _: NonZeroU32) {
        core::assert_eq!(context, self.context);
        self.accesses.push(ContextAccess::Complete(context));
    }
}

impl super::PlicContextRegisters for ContextModel {
    fn init_by_context(&mut self, context: usize) {
        core::assert_eq!(context, self.context);
        self.threshold = 0;
        self.accesses.push(ContextAccess::InitContext(context));
    }
}

#[def_test]
fn plic_register_layout_owns_enable_stride_and_aperture() {
    assert_eq!(super::registers::PlicRegisters::enable_context_end(0), Some(0x2080));
    assert_eq!(super::registers::PlicRegisters::enable_context_end(1), Some(0x2100));
    assert_eq!(super::registers::PlicRegisters::required_aperture(0), Some(0x200008));
    assert_eq!(super::registers::PlicRegisters::required_aperture(1), Some(0x201008));
    assert_eq!(
        super::registers::PlicRegisters::validate_context(1, 0x2000),
        None
    );
    assert_eq!(
        super::registers::PlicRegisters::validate_context(
            super::registers::MAX_CONTEXT,
            0x0400_0000
        ),
        None
    );
}

struct RegisterFixture {
    _words: Vec<u32>,
    base: *mut u32,
    registers: super::registers::PlicRegisters,
}

impl RegisterFixture {
    fn new() -> Self {
        let mut words = alloc::vec![0u32; 0x202008 / 4 + 2];
        let base = words.as_mut_ptr();
        let registers = {
            // SAFETY: `words` provides an aligned live backing store of
            // `words.len() * 4` bytes; the vector is retained and never
            // reallocated while the register view exists.
            unsafe {
                super::registers::PlicRegisters::new(
                    NonNull::new(base.cast::<u8>()).unwrap(),
                    words.len() * 4,
                )
                .unwrap()
            }
        };
        Self {
            _words: words,
            base,
            registers,
        }
    }

    fn word(&self, index: usize) -> u32 {
        let register = self.base.wrapping_add(index);
        // SAFETY: `index` stays inside the retained backing allocation, and no
        // Rust reference to that allocation is live during this volatile read.
        unsafe { register.read_volatile() }
    }

    fn set_word(&mut self, index: usize, value: u32) {
        let register = self.base.wrapping_add(index);
        // SAFETY: `index` stays inside the retained backing allocation, and no
        // Rust reference to that allocation is live during this volatile write.
        unsafe { register.write_volatile(value) };
    }
}

#[def_test]
fn plic_register_window_rejects_unaligned_and_invalid_lengths() {
    let mut words = [0u32; 4];
    let aligned = NonNull::new(words.as_mut_ptr().cast::<u8>()).unwrap();
    let unaligned = NonNull::new(words.as_mut_ptr().cast::<u8>().wrapping_add(1)).unwrap();
    let unaligned_result = {
        // SAFETY: the pointer is non-null and the length is bounded by `words`;
        // construction must reject the unaligned base.
        unsafe { super::registers::PlicRegisters::new(unaligned, 8) }
    };
    assert!(matches!(
        unaligned_result,
        Err(super::registers::PlicRegisterError::UnalignedBase)
    ));
    let invalid_length = {
        // SAFETY: see above; only the length is deliberately invalid here.
        unsafe { super::registers::PlicRegisters::new(aligned, 6) }
    };
    assert!(matches!(
        invalid_length,
        Err(super::registers::PlicRegisterError::InvalidWindow)
    ));
    let valid = {
        // SAFETY: aligned non-null storage with a whole number of registers.
        unsafe { super::registers::PlicRegisters::new(aligned, 8) }
    };
    assert!(valid.is_ok());
}

#[def_test]
fn plic_register_access_uses_owned_layout_and_preserves_adjacent_bits() {
    let mut fixture = RegisterFixture::new();
    let source1 = NonZeroU32::new(1).unwrap();
    let source30 = NonZeroU32::new(30).unwrap();
    let source31 = NonZeroU32::new(31).unwrap();
    let source32 = NonZeroU32::new(32).unwrap();
    let source9 = NonZeroU32::new(9).unwrap();

    fixture.registers.write_priority(source31, 0x11);
    fixture.registers.write_priority(source32, 0x22);
    assert_eq!(fixture.word(31), 0x11);
    assert_eq!(fixture.word(32), 0x22);
    assert_eq!(fixture.registers.read_priority(source31), 0x11);
    assert_eq!(fixture.registers.read_priority(source32), 0x22);

    fixture.registers.set_enabled(source31, 0, true);
    assert!(fixture.registers.is_enabled(source31, 0));
    assert!(!fixture.registers.is_enabled(source32, 0));
    assert_eq!(fixture.word(0x2000 / 4) & (1 << 31), 1 << 31);
    fixture.registers.set_enabled(source32, 0, true);
    assert!(fixture.registers.is_enabled(source31, 0));
    assert!(fixture.registers.is_enabled(source32, 0));
    assert_eq!(fixture.word(0x2000 / 4 + 1) & 1, 1);

    fixture.registers.set_enabled(source30, 0, true);
    assert!(fixture.registers.is_enabled(source30, 0));
    fixture.registers.set_enabled(source30, 0, false);
    assert!(!fixture.registers.is_enabled(source30, 0));
    assert!(fixture.registers.is_enabled(source31, 0));
    assert_eq!(fixture.word(0x2000 / 4) & (1 << 31), 1 << 31);

    fixture.registers.set_enabled(source1, 3, true);
    assert!(fixture.registers.is_enabled(source1, 3));
    assert_eq!(fixture.word((0x2000 + 3 * 0x80) / 4) & 2, 2);
    assert!(!fixture.registers.is_enabled(source1, 0));

    let context2 = 0x20_0000 + 2 * 0x1000;
    fixture.set_word(context2 / 4, u32::MAX);
    fixture.registers.init_by_context(2);
    assert_eq!(fixture.word(context2 / 4), 0);
    fixture.set_word(context2 / 4 + 1, 7);
    assert_eq!(
        fixture.registers.claim(2).map(|source| source.get()),
        Some(7)
    );
    fixture.registers.complete(2, source9);
    assert_eq!(fixture.word(context2 / 4 + 1), 9);

    assert_eq!(
        super::registers::PlicRegisters::validate_context(0, 0x1000),
        None
    );
    assert_eq!(
        super::registers::PlicRegisters::validate_context(0, 0x200008),
        Some(0x200008)
    );
}

#[def_test]
fn plic_contexts_preserve_raw_indices_across_sparse_and_mixed_entries() {
    let cpu0 = cells(&[0x100]);
    let cpu1 = cells(&[0x105]);
    let bytes = plic_context_fixture(
        &[(1, Some(&cpu0)), (2, Some(&cpu1))],
        &[2, 9, 1, 0x0b, 1, 9, 2, 0x0b],
        0x0400_0000,
    );
    let has_second_cpu = kbuild_config::NR_CPUS >= 2;
    let contexts = parsed_contexts(&bytes, |hardware_cpu_id| match hardware_cpu_id {
        0x100 => Some(LogicalCpuId::new(0)),
        0x105 if has_second_cpu => Some(LogicalCpuId::new(1)),
        _ => None,
    })
    .unwrap();
    assert_eq!(contexts.get(LogicalCpuId::new(0)).unwrap().index, 2);
    if has_second_cpu {
        assert_eq!(contexts.get(LogicalCpuId::new(1)).unwrap().index, 0);
    }
    assert_eq!(contexts.iter().count(), if has_second_cpu { 2 } else { 1 });
}

#[def_test]
fn plic_contexts_skip_argument_placeholder_without_compacting_indices() {
    let cpu = cells(&[0x100]);
    let bytes = plic_context_fixture(
        &[(1, Some(&cpu))],
        &[1, u32::MAX, 1, 9],
        0x0400_0000,
    );
    let declarations = parse_declarations(&bytes).unwrap();
    assert_eq!(declarations.len(), 1);
    assert_eq!(declarations[0].index, 1);
    let contexts = build_contexts(&declarations, 0x0400_0000, |_| {
        Some(LogicalCpuId::new(0))
    })
    .unwrap();
    assert_eq!(contexts.get(LogicalCpuId::new(0)).unwrap().index, 1);
}

#[def_test]
fn plic_contexts_skip_hardware_ids_outside_the_runtime_cpu_set() {
    let cpu0 = cells(&[0x100]);
    let cpu1 = cells(&[0x105]);
    let bytes = plic_context_fixture(
        &[(1, Some(&cpu0)), (2, Some(&cpu1))],
        &[1, 9, 2, 9],
        0x0400_0000,
    );
    let contexts = parsed_contexts(&bytes, |hardware_cpu_id| {
        (hardware_cpu_id == 0x100).then_some(LogicalCpuId::new(0))
    })
    .unwrap();
    assert_eq!(contexts.iter().count(), 1);
    assert_eq!(contexts.get(LogicalCpuId::new(0)).unwrap().index, 0);
}

#[def_test]
fn plic_context_parsing_rejects_missing_identities_and_bad_phandles() {
    let cpu = cells(&[0x100]);
    let missing = plic_context_fixture(&[(1, None)], &[1, 9], 0x0400_0000);
    assert!(matches!(
        parse_declarations(&missing),
        Err(super::firmware::PlicConfigError::MissingCpuIdentity { index: 0, .. })
    ));

    let bad_phandle = plic_context_fixture(
        &[(1, Some(&cpu))],
        &[9, 9],
        0x0400_0000,
    );
    assert!(matches!(
        parse_declarations(&bad_phandle),
        Err(super::firmware::PlicConfigError::Context { index: 0, .. })
    ));
}

#[def_test]
fn plic_context_build_rejects_duplicates_and_empty_results() {
    let cpu = cells(&[0x100]);
    let duplicate = plic_context_fixture(
        &[(1, Some(&cpu))],
        &[1, 9, 1, 9],
        0x0400_0000,
    );
    let declarations = parse_declarations(&duplicate).unwrap();
    assert!(matches!(
        build_contexts(&declarations, 0x0400_0000, |_| Some(LogicalCpuId::new(0))),
        Err(super::context::PlicContextError::Conflicting { index: 1 })
    ));

    let no_context = plic_context_fixture(&[(1, Some(&cpu))], &[1, 0x0b], 0x0400_0000);
    let declarations = parse_declarations(&no_context).unwrap();
    assert!(matches!(
        build_contexts(&declarations, 0x0400_0000, |_| Some(LogicalCpuId::new(0))),
        Err(super::context::PlicContextError::NoContext)
    ));
}

#[def_test]
fn plic_context_build_validates_aperture_before_register_access() {
    let cpu = cells(&[0]);
    let bytes = plic_context_fixture(
        &[(1, Some(&cpu))],
        &[1, 0x0b, 1, 9],
        0x2000,
    );
    let declarations = parse_declarations(&bytes).unwrap();
    assert!(matches!(
        build_contexts(&declarations, 0x2000, |_| Some(LogicalCpuId::new(0))),
        Err(super::context::PlicContextError::InvalidAperture {
            index: 1,
            required: 0x20_1008,
        })
    ));
}

#[def_test]
fn plic_context_boundary_checks_are_nonmutating() {
    let last_context = super::context::PlicContext {
        logical_cpu: LogicalCpuId::new(0),
        hardware_cpu_id: 0x100,
        index: super::registers::MAX_CONTEXT - 1,
    };
    let mut last = super::context::PlicContexts::new();
    assert_eq!(last.insert(last_context), Ok(()));

    let first_invalid = super::context::PlicContext {
        logical_cpu: LogicalCpuId::new(0),
        hardware_cpu_id: 0x101,
        index: super::registers::MAX_CONTEXT,
    };
    let mut invalid = super::context::PlicContexts::new();
    let before = invalid;
    assert_eq!(invalid.insert(first_invalid), Err(()));
    assert_eq!(invalid, before);
}

#[def_test]
fn production_context_preparation_checks_duplicate_and_register_write_order() {
    let context = super::context::PlicContext {
        logical_cpu: LogicalCpuId::new(0),
        hardware_cpu_id: 0x100,
        index: 3,
    };
    let mut model = ContextModel::new(3, false);
    let mut prepared = KCpuMask::new();
    assert_eq!(
        super::prepare_plic_context(
            &mut model,
            &mut prepared,
            LogicalCpuId::new(0),
            context,
            super::PlicSources { last: 7 },
        ),
        Ok(())
    );
    assert_eq!(model.threshold, 0);
    assert!(prepared.get_logical(LogicalCpuId::new(0)));
    let mut expected: Vec<_> = (0..7).map(|_| ContextAccess::Enable(false)).collect();
    expected.push(ContextAccess::InitContext(3));
    assert_eq!(model.accesses, expected);

    let accesses = model.accesses.len();
    assert_eq!(
        super::prepare_plic_context(
            &mut model,
            &mut prepared,
            LogicalCpuId::new(0),
            context,
            super::PlicSources { last: 7 },
        ),
        Err("PLIC context already initialized")
    );
    assert_eq!(model.accesses.len(), accesses);
    assert!(prepared.get_logical(LogicalCpuId::new(0)));
}

#[def_test]
fn completion_uses_the_context_selected_from_the_table() {
    let declarations = [super::context::PlicContextDecl {
        index: 2,
        hardware_cpu_id: 0x100,
    }];
    let contexts = build_contexts(&declarations, 0x0400_0000, |_| {
        Some(LogicalCpuId::new(0))
    })
    .unwrap();
    let context = contexts.get(LogicalCpuId::new(0)).unwrap();
    assert_eq!(context.index, 2);

    let source = NonZeroU32::new(5).unwrap();
    let mut model = ContextModel::new(context.index, false);
    super::complete_plic_claim(&mut model, context.index, source);
    assert_eq!(
        model.accesses,
        [
            ContextAccess::Enable(true),
            ContextAccess::Complete(2),
            ContextAccess::Enable(false),
        ]
    );
    assert!(!model.enabled);
}

#[def_test]
fn legacy_plic_contexts_preserve_the_fixed_hart_layout() {
    let declarations = super::legacy::declarations().unwrap();
    let contexts = build_contexts(&declarations, super::PLIC_MMIO_SIZE, |hardware_cpu_id| {
        let raw = usize::try_from(hardware_cpu_id).ok()?;
        kcpu_id_map::logical_cpu_id(kcpu_id_map::RawCpuId::new(raw))
    })
    .unwrap();
    let logical_cpu = kplat::cpu::id();
    let hardware_cpu_id = kcpu_id_map::raw_cpu_id(logical_cpu).unwrap().as_usize();
    let context = contexts.get(logical_cpu).unwrap();
    assert_eq!(context.index, hardware_cpu_id * 2 + 1);
    assert_eq!(
        context.hardware_cpu_id,
        u64::try_from(hardware_cpu_id).unwrap()
    );
    assert_eq!(contexts.iter().count(), kcpu_id_map::nr_cpus());
}

#[def_test]
fn plic_cells_decode_level_high_sources_and_reject_invalid_numbers() {
    use kirq::{IrqDomainXlate, IrqTrigger, IrqHwSpec, IrqXlateError};

    let xlate = &super::PLIC_XLATE;
    assert_eq!(
        xlate.translate(&[1]),
        Ok(IrqHwSpec {
            hwirq: 1,
            trigger: IrqTrigger::LevelHigh,
        })
    );
    assert_eq!(
        xlate.translate(&[1023]),
        Ok(IrqHwSpec {
            hwirq: 1023,
            trigger: IrqTrigger::LevelHigh,
        })
    );
    for cells in [&[][..], &[1, 2][..]] {
        assert_eq!(xlate.translate(cells), Err(IrqXlateError::Malformed));
    }
    for source in [0, 1024, u32::MAX] {
        assert_eq!(
            xlate.translate(&[source]),
            Err(IrqXlateError::InvalidHwirq)
        );
    }
}
