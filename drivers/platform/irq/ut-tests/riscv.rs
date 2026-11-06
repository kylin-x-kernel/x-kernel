// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use riscv::register::sie;
use unittest::{assert, assert_eq, def_test};

use super::{NonZeroU32, PlicCompletion, complete_plic_claim};

#[def_test]
fn plic_source_count_requires_one_supported_nonzero_cell() {
    use super::PlicSources;
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
    let sources = super::PlicSources::from_property(Some(&53u32.to_be_bytes())).unwrap();
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
    super::disable_plic_sources(&mut model, 3, super::PlicSources { last: 7 });
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
    let sources = Some(super::PlicSources { last: 53 });
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
