// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC interrupt controller integration for AArch64 platforms.

use core::{fmt, sync::atomic::Ordering};

#[path = "gicv2.rs"]
mod gicv2;
#[path = "gicv3.rs"]
mod gicv3;
#[path = "gic_spi.rs"]
mod spi;

use kirq::TargetCpu;
use lazyinit::LazyInit;
use memaddr::{PhysAddr, VirtAddr};
use memspace::iomap_device;

const GICD_NAME: &str = "gicd";
const GICC_NAME: &str = "gicc";
const GICR_NAME: &str = "gicr";
static GIC_INFO: LazyInit<GicConfig> = LazyInit::new();
static ACTIVE_GIC: LazyInit<GicVersion> = LazyInit::new();
static STANDARD_INTID_LIMIT: LazyInit<usize> = LazyInit::new();

fn standard_intid_limit(typer: u32) -> usize {
    // ITLinesNumber includes the private prefix. IDbits also covers other
    // INTID classes and cannot bound the standard distributor register banks.
    (((typer as usize & 0x1f) + 1) * 32).min(1020)
}

fn remember_standard_intid_limit(typer: u32) {
    STANDARD_INTID_LIMIT.init_once(standard_intid_limit(typer));
}

fn select_gic_flow(
    desc: kirq::IrqDesc,
    limit: Option<usize>,
) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
    use kirq::{IrqController, IrqDescError, IrqFlow, IrqTrigger};

    if desc.domain.is_none()
        && desc.controller == IrqController::Unknown
        && desc.hwirq < 32
        && desc.hwirq != 27
        && !desc.flags.contains(kirq::IrqFlags::PER_CPU)
    {
        return Ok(IrqFlow::PerCpu);
    }

    // Scope this admission to explicit GIC resources. Directory-only and
    // unmigrated legacy identities keep their existing provider contract.
    if desc.domain != Some(GIC_ROOT_DOMAIN) && desc.controller != IrqController::Gic {
        return Ok(IrqFlow::Legacy);
    }
    if desc.domain.is_some_and(|domain| domain != GIC_ROOT_DOMAIN)
        || !matches!(desc.controller, IrqController::Unknown | IrqController::Gic)
        || limit.is_none_or(|limit| desc.hwirq >= limit)
    {
        return Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    // Linux gic_set_type rejects negative-polarity SPI requests rather than
    // silently programming their positive counterpart. Private sources retain
    // their separate contract; unspecified triggers do not program ICFGR.
    if desc.hwirq >= 32 && matches!(desc.trigger, IrqTrigger::EdgeFalling | IrqTrigger::LevelLow) {
        return Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    // NMI registration tags its fallback PER_CPU and retains its separate
    // lifecycle. Ordinary PPIs, including explicitly forwarded PPIs, use
    // CPU-local flow state rather than shared-source running/pending state.
    if desc.flags.contains(kirq::IrqFlags::PER_CPU) {
        return Ok(IrqFlow::Legacy);
    }
    if desc.hwirq < 32 {
        return Ok(IrqFlow::PerCpu);
    }
    let ordinary_flags = kirq::IrqFlags::SHARED | kirq::IrqFlags::WAKEUP_SOURCE;
    if !(desc.flags - ordinary_flags).is_empty()
        || desc.polarity == kirq::IrqPolarity::Low
        || desc.affinity != kirq::IrqAffinity::Any
    {
        return Err(IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
    }
    Ok(IrqFlow::Fasteoi(&spi::CHIP))
}

kcpu_slot::cpu_slot! {
    static LOCAL_INIT_STARTED: core::sync::atomic::AtomicBool =
        core::sync::atomic::AtomicBool::new(false);
}

unittest::path_mod_test!(tests, "../ut-tests/gic.rs");

pub const GIC_ROOT_DOMAIN: kirq::IrqDomainId = kirq::GIC_ROOT_DOMAIN;

pub const fn irq_desc(hwirq: usize, trigger: kirq::IrqTrigger) -> kirq::IrqDesc {
    kirq::gic_irq_desc(hwirq, trigger)
}

pub const fn level_irq_desc(hwirq: usize) -> kirq::IrqDesc {
    kirq::gic_level_irq_desc(hwirq)
}

pub const fn edge_irq_desc(hwirq: usize) -> kirq::IrqDesc {
    kirq::gic_edge_irq_desc(hwirq)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GicVersion {
    V2,
    V3,
}

impl fmt::Display for GicVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::V2 => "GICv2",
            Self::V3 => "GICv3",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicMmioRegion {
    pub paddr: PhysAddr,
    pub size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GicConfig {
    pub version: GicVersion,
    pub gicd: GicMmioRegion,
    pub gicc: Option<GicMmioRegion>,
    pub gicr: Option<GicMmioRegion>,
    pub gich: Option<GicMmioRegion>,
    pub gicv: Option<GicMmioRegion>,
}

impl GicConfig {
    fn map_gicd(self) -> VirtAddr {
        map_mmio_region(self.gicd, GICD_NAME)
    }

    fn map_gicc(self) -> VirtAddr {
        map_mmio_region(
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: GICv2 has no \
                          CPU-interface region; interrupt-controller initialization has no error \
                          return or recovery"
            )]
            self.gicc.expect("missing GICC region"),
            GICC_NAME,
        )
    }

    fn map_gicr(self) -> VirtAddr {
        map_mmio_region(
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: GICv3 has no \
                          redistributor region; interrupt-controller initialization has no error \
                          return or recovery"
            )]
            self.gicr.expect("missing GICR region"),
            GICR_NAME,
        )
    }
}

#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: a firmware-described GIC register \
              region cannot be mapped; interrupt-controller initialization has no error return or \
              recovery"
)]
fn map_mmio_region(region: GicMmioRegion, name: &'static str) -> VirtAddr {
    iomap_device(region.paddr, region.size, name)
        .unwrap_or_else(|err| panic!("failed to map {name}: {err:?}"))
}

fn is_gicv2_compatible(compatible: &str) -> bool {
    matches!(
        compatible,
        "arm,gic-400"
            | "arm,cortex-a15-gic"
            | "arm,cortex-a9-gic"
            | "arm,cortex-a7-gic"
            | "arm,gic-v2"
    )
}

fn is_gicv3_compatible(compatible: &str) -> bool {
    matches!(compatible, "arm,gic-v3" | "arm,gic-v4")
}

fn config_from_device_tree() -> Option<GicConfig> {
    let node = of::fdt()?.all_nodes().find(|node| {
        node.property("interrupt-controller").is_some()
            && node.compatibles().any(|compatible| {
                is_gicv2_compatible(compatible) || is_gicv3_compatible(compatible)
            })
    })?;

    let version = if node.compatibles().any(is_gicv3_compatible) {
        GicVersion::V3
    } else if node.compatibles().any(is_gicv2_compatible) {
        GicVersion::V2
    } else {
        return None;
    };

    let mut regs = node.reg()?;
    let gicd = regs.next()?;
    let gicd = GicMmioRegion {
        paddr: PhysAddr::from_usize(gicd.starting_address as usize),
        size: gicd.size,
    };

    match version {
        GicVersion::V2 => {
            let gicc = regs.next()?;
            let gich = regs.next().map(|reg| GicMmioRegion {
                paddr: PhysAddr::from_usize(reg.starting_address as usize),
                size: reg.size,
            });
            let gicv = regs.next().map(|reg| GicMmioRegion {
                paddr: PhysAddr::from_usize(reg.starting_address as usize),
                size: reg.size,
            });
            Some(GicConfig {
                version,
                gicd,
                gicc: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicc.starting_address as usize),
                    size: gicc.size,
                }),
                gicr: None,
                gich,
                gicv,
            })
        }
        GicVersion::V3 => {
            let gicr = regs.next()?;
            Some(GicConfig {
                version,
                gicd,
                gicc: None,
                gich: None,
                gicv: None,
                gicr: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicr.starting_address as usize),
                    size: gicr.size,
                }),
            })
        }
    }
}

fn guest_interface(config: GicConfig) -> Option<kirq::GuestIrqInterface> {
    if config.version != GicVersion::V2 {
        return None;
    }
    let control = config.gich?;
    let cpu = config.gicv?;
    let regions = [config.gicd, config.gicc?, control, cpu];
    for (index, region) in regions.iter().enumerate() {
        let start = region.paddr.as_usize();
        let end = start.checked_add(region.size)?;
        if !start.is_multiple_of(4096) || region.size == 0 {
            return None;
        }
        for other in &regions[..index] {
            let other_start = other.paddr.as_usize();
            let other_end = other_start.checked_add(other.size)?;
            if start < other_end && other_start < end {
                return None;
            }
        }
    }
    if control.size < 0x1000 || cpu.size < 0x2000 {
        return None;
    }
    Some(kirq::GuestIrqInterface::GicV2 {
        control_paddr: control.paddr.as_usize(),
        cpu_paddr: cpu.paddr.as_usize(),
    })
}

fn remember_config(config: GicConfig) {
    if let Some(saved) = GIC_INFO.get() {
        assert_eq!(*saved, config, "GIC config changed after initialization");
    } else {
        GIC_INFO.init_once(config);
    }
}

pub fn set_config(config: GicConfig) {
    remember_config(config);
}

pub fn active_version() -> GicVersion {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: GIC initialization records its \
                  configuration before publishing interrupt operations"
    )]
    ACTIVE_GIC
        .get()
        .copied()
        .or_else(|| GIC_INFO.get().copied().map(|config| config.version))
        .expect("GIC config not initialized")
}

/// Initializes the controller and the primary CPU interface.
///
/// Call during boot with local CPU IRQ delivery masked and before activating
/// interrupt sources. This includes the local setup described by
/// [`init_current_cpu`]; it is not a runtime reinitialization operation.
///
/// # Errors
/// Returns a local CPU initialization or boot-context error. Global controller
/// setup may already have taken effect; an error does not roll it back.
///
/// # Panics
/// Panics on conflicting configuration, invalid MMIO configuration or a fatal
/// controller-wide initialization failure.
pub fn init(config: GicConfig) -> Result<(), &'static str> {
    remember_config(config);
    if let Some(active) = ACTIVE_GIC.get() {
        assert_eq!(
            *active, config.version,
            "GIC version changed after initialization"
        );
    } else {
        ACTIVE_GIC.init_once(config.version);
    }

    match config.version {
        GicVersion::V2 => gicv2::init(config.map_gicd(), config.map_gicc()),
        GicVersion::V3 => gicv3::init(config.map_gicd(), config.map_gicr()),
    }

    init_current_cpu()
}

/// Discovers the GIC and initializes it for the primary CPU.
///
/// Requires the same masked boot context as [`init`].
///
/// # Errors
/// Returns an error if configuration is unavailable or local CPU setup fails.
///
/// # Panics
/// Has the same controller-wide failure conditions as [`init`].
pub fn init_from_device_tree() -> Result<(), &'static str> {
    let config = config_from_device_tree().ok_or("failed to parse GIC from device tree")?;
    init(config)
}

/// Initializes this CPU's GIC interface during bring-up.
///
/// Keep local CPU IRQ delivery masked throughout this call and do not migrate
/// execution. In pseudo-NMI builds, DAIF.I must still be set on entry; this call
/// establishes PMR masking but does not enable IRQ delivery.
///
/// [`init`] (including [`init_from_device_tree`]) already calls this for the
/// primary CPU. Secondary CPUs call it once before activating local sources.
/// This is not an idempotent readiness check: it resets interface priorities
/// and private interrupt enable/active state. GICv2 SGI enable bits may be
/// read-as-one/write-ignored; CPU IRQ masking protects the preparation phase.
///
/// # Errors
/// Returns a GICv3 CPU-interface initialization error before setting EOI mode
/// or publishing PMR readiness. Partial hardware setup is not rolled back;
/// callers must not activate sources or continue CPU bring-up after failure.
/// Also rejects IRQ-enabled entry or a repeated initialization attempt before
/// touching local controller registers, including retries after failure.
///
/// # Panics
/// Panics if global controller initialization has not run.
pub fn init_current_cpu() -> Result<(), &'static str> {
    let version = active_version();
    if karch::local_irq_enabled() {
        return Err("local GIC initialization requires IRQs masked");
    }
    // The boot caller excludes migration. Claim before any destructive MMIO;
    // an unsuccessful attempt stays claimed because hardware is not rolled back.
    if LOCAL_INIT_STARTED
        .current_shared()
        .swap(true, Ordering::Relaxed)
    {
        return Err("local GIC initialization already attempted");
    }
    match version {
        GicVersion::V2 => gicv2::init_current_cpu(),
        GicVersion::V3 => {
            gicv3::init_current_cpu()?;
            // PMR (ICC_PMR_EL1) only exists on GICv3; on a degraded build
            // (GICv2) it must never be touched, so only mark the CPU
            // interface PMR-ready here, after its initialization guard is gone.
            #[cfg(feature = "nmi-pseudo")]
            karch::pmr::initialize_masked();
        }
    }
    Ok(())
}

pub fn set_trigger(interrupt_id: usize, edge: bool) {
    match active_version() {
        GicVersion::V2 => gicv2::set_trigger(interrupt_id, edge),
        GicVersion::V3 => gicv3::set_trigger(interrupt_id, edge),
    }
}

pub fn enable(irq: usize, enabled: bool) {
    match active_version() {
        GicVersion::V2 => gicv2::enable(irq, enabled),
        GicVersion::V3 => gicv3::enable(irq, enabled),
    }
}

/// Claim, classify, and dispatch an interrupt taken from an IRQ-on context.
///
/// GICv3 uses the post-ack running priority, or the hardware-NMI acknowledge
/// register, to select kirq's generic IRQ or NMI handler. GICv2 always uses the
/// generic IRQ handler. The return value is resolved normal-IRQ metadata for
/// the outer deferred hook, not a claim classification.
fn gic_dispatch_irq_from_irqson(_unused: usize) -> Option<kirq::Virq> {
    match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq_from_irqson(),
        GicVersion::V3 => gicv3::dispatch_irq_from_irqson(),
    }
}

/// Dispatch an NMI taken from a context with IRQs **disabled**.
///
/// Mirrors Linux `__gic_handle_irq_from_irqsoff`.  Dispatches to the
/// version‑specific irqsoff backend.  GICv3 saves/lowers/restores PMR around
/// the ack; GICv2 merely acks (no pseudo‑NMI support). It does **not** open the
/// NMI window. A valid claim is dispatched and completed through kirq's generic
/// NMI helper before this function returns.
fn gic_dispatch_nmi_from_irqsoff(_unused: usize) {
    let claim = match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq_from_irqsoff(),
        GicVersion::V3 => gicv3::dispatch_irq_from_irqsoff(),
    };
    if let Some((hwirq, completion_cookie)) = claim {
        kirq::generic_handle_nmi(kirq::DispatchedIrq::new(hwirq, completion_cookie));
    }
}

/// Deactivate a previously dispatched interrupt.
/// PMR is restored by the exception exit path (saved at entry).
pub fn complete_irq(completion_cookie: usize) {
    match active_version() {
        GicVersion::V2 => gicv2::complete_irq(completion_cookie),
        GicVersion::V3 => gicv3::complete_irq(completion_cookie),
    }
}

pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match active_version() {
        GicVersion::V2 => gicv2::notify_cpu(interrupt_id, target),
        GicVersion::V3 => gicv3::notify_cpu(interrupt_id, target),
    }
}

/// Runs a bounded, non-blocking test action while the real GICv3 lock is held.
///
/// Requires initialized GIC and local irq_work readiness. The action may submit
/// irq_work or arrange an NMI, but must not wait for the ordinary IRQ to drain
/// while this lock masks it. Returns `false` without running the action on GICv2.
#[cfg(unittest)]
pub fn with_irq_work_gic_lock_for_test(action: impl FnOnce()) -> bool {
    match active_version() {
        GicVersion::V2 => false,
        GicVersion::V3 => {
            gicv3::with_irq_work_gic_lock_for_test(action);
            true
        }
    }
}

/// Samples the current CPU's GICv2 PPI for kernel integration tests.
/// Returns `(enabled, active, forwarded)`, or `None` for other controllers/IDs.
#[cfg(unittest)]
pub fn local_ppi_state_for_test(irq: usize) -> Option<(bool, bool, bool)> {
    if ACTIVE_GIC.get() != Some(&GicVersion::V2) || !(16..32).contains(&irq) {
        return None;
    }
    Some(gicv2::local_ppi_state_for_test(irq))
}

/// Pends a GICv2 PPI on the executing CPU for kernel integration tests.
/// Returns `false` for other controllers or non-PPI IDs.
///
/// # Safety
/// The caller owns this source, has stopped its producer, and excludes local
/// IRQs and migration. It must drain delivery and restore source state before
/// relinquishing that ownership.
#[cfg(unittest)]
pub unsafe fn pend_local_ppi_for_test(irq: usize) -> bool {
    if ACTIVE_GIC.get() != Some(&GicVersion::V2) || !(16..32).contains(&irq) {
        return false;
    }
    gicv2::pend_local_ppi_for_test(irq);
    true
}

pub fn set_prio(irq: usize, priority: u8) {
    // NMI classification relies on the invariant that only NMI sources live
    // below IRQ_THRESHOLD: excp routes IRQs taken with PMR <= NMI_ONLY to the
    // NMI path, and GICv3 treats RPR == 0 as NMI.  Programming a normal IRQ
    // into that range would silently drop it as an "Unhandled NMI" whenever
    // it fires inside a critical section.
    assert!(
        priority == 0 || priority >= karch::pmr::IRQ_THRESHOLD,
        "set_prio({irq}, {priority:#x}): non-NMI priority must be >= {:#x}",
        karch::pmr::IRQ_THRESHOLD
    );
    match active_version() {
        GicVersion::V2 => gicv2::set_prio(irq, priority),
        GicVersion::V3 => gicv3::set_prio(irq, priority),
    }
}

/// Return whether the active GIC advertises GICv3.3 NMI support.
#[cfg(feature = "nmi-hardware")]
pub fn supports_hardware_nmi() -> bool {
    match active_version() {
        GicVersion::V2 => false,
        GicVersion::V3 => gicv3::supports_hardware_nmi(),
    }
}

/// Set or clear the GICv3.3 NMI attribute for an interrupt line.
///
/// Returns `false` when the active GIC cannot program the attribute for
/// this line (GICv2, unsupported INTID, or missing redistributor frame).
#[cfg(feature = "nmi-hardware")]
pub fn set_nmi_attr(irq: usize, nmi: bool) -> bool {
    match active_version() {
        GicVersion::V2 => {
            warn!("hardware NMI is not supported on GICv2");
            false
        }
        GicVersion::V3 => gicv3::set_nmi_attr(irq, nmi),
    }
}

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn guest_interface() -> Option<kirq::GuestIrqInterface> {
        if ACTIVE_GIC.get() != Some(&GicVersion::V2) {
            return None;
        }
        guest_interface(*GIC_INFO.get()?)
    }

    fn percpu_forwarding(desc: kirq::IrqDesc) -> Option<&'static kirq::PerCpuForwardingChip> {
        (ACTIVE_GIC.get() == Some(&GicVersion::V2)
            && desc.domain == Some(GIC_ROOT_DOMAIN)
            && (16..32).contains(&desc.hwirq)
            && select_gic_flow(desc, STANDARD_INTID_LIMIT.get().copied())
                == Ok(kirq::IrqFlow::PerCpu))
        .then_some(&gicv2::FORWARDING)
    }

    fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
        select_gic_flow(desc, STANDARD_INTID_LIMIT.get().copied())
    }

    fn prepare_local_irq_work() -> Option<fn()> {
        match active_version() {
            GicVersion::V2 => None,
            GicVersion::V3 => gicv3::prepare_local_irq_work(),
        }
    }

    fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
        match active_version() {
            GicVersion::V2 => gicv2::configure(desc),
            GicVersion::V3 => gicv3::configure(desc),
        }
    }

    fn enable(desc: kirq::IrqDesc, enabled: bool) {
        crate::gic::enable(desc.hwirq, enabled);
    }

    /// IRQ path: IRQs were enabled → may be IRQ or NMI.
    ///
    /// The GIC owns claim-time IRQ/NMI classification and invokes the matching
    /// generic handler before returning to the common IRQ-entry tail.
    fn dispatch_irq(irq: usize) -> Option<kirq::Virq> {
        crate::gic::gic_dispatch_irq_from_irqson(irq)
    }

    /// NMI path: IRQs were disabled → no lock, no NMI window, PMR protected.
    fn dispatch_nmi(irq: usize) {
        crate::gic::gic_dispatch_nmi_from_irqsoff(irq);
    }

    /// Deactivate the interrupt.  PMR is restored by exception exit.
    fn complete_irq(completion_cookie: usize) {
        crate::gic::complete_irq(completion_cookie);
    }

    fn notify_cpu(interrupt_id: usize, target: kirq::TargetCpu) {
        crate::gic::notify_cpu(interrupt_id, target);
    }

    fn set_prio(irq: usize, priority: u8) {
        crate::gic::set_prio(irq, priority);
    }
}
