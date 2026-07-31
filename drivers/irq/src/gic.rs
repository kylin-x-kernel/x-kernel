// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC interrupt controller integration for AArch64 platforms.

#[path = "gicv2.rs"]
mod gicv2;
#[path = "gicv3.rs"]
mod gicv3;

use khal::irq::TargetCpu;
use lazyinit::LazyInit;
use memaddr::{PhysAddr, VirtAddr};
use memspace::iomap_device;

const GICD_NAME: &str = "gicd";
const GICC_NAME: &str = "gicc";
const GICR_NAME: &str = "gicr";
static GIC_INFO: LazyInit<GicConfig> = LazyInit::new();
static ACTIVE_GIC: LazyInit<GicVersion> = LazyInit::new();
pub const GIC_ROOT_DOMAIN: khal::irq::IrqDomainId = khal::irq::GIC_ROOT_DOMAIN;

pub const fn irq_desc(hwirq: usize, trigger: khal::irq::IrqTrigger) -> khal::irq::IrqDesc {
    khal::irq::gic_irq_desc(hwirq, trigger)
}

pub const fn level_irq_desc(hwirq: usize) -> khal::irq::IrqDesc {
    khal::irq::gic_level_irq_desc(hwirq)
}

pub const fn edge_irq_desc(hwirq: usize) -> khal::irq::IrqDesc {
    khal::irq::gic_edge_irq_desc(hwirq)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GicVersion {
    V2,
    V3,
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
}

impl GicConfig {
    fn map_gicd(self) -> VirtAddr {
        map_mmio_region(self.gicd, GICD_NAME)
    }

    fn map_gicc(self) -> VirtAddr {
        map_mmio_region(self.gicc.expect("missing GICC region"), GICC_NAME)
    }

    fn map_gicr(self) -> VirtAddr {
        map_mmio_region(self.gicr.expect("missing GICR region"), GICR_NAME)
    }
}

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
            Some(GicConfig {
                version,
                gicd,
                gicc: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicc.starting_address as usize),
                    size: gicc.size,
                }),
                gicr: None,
            })
        }
        GicVersion::V3 => {
            let gicr = regs.next()?;
            Some(GicConfig {
                version,
                gicd,
                gicc: None,
                gicr: Some(GicMmioRegion {
                    paddr: PhysAddr::from_usize(gicr.starting_address as usize),
                    size: gicr.size,
                }),
            })
        }
    }
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

fn active_version() -> GicVersion {
    ACTIVE_GIC
        .get()
        .copied()
        .or_else(|| GIC_INFO.get().copied().map(|config| config.version))
        .expect("GIC config not initialized")
}

pub fn init(config: GicConfig) {
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

    init_current_cpu();
}

pub fn init_from_device_tree() {
    let config = config_from_device_tree().expect("failed to parse GIC from device tree");
    init(config);
}

pub fn init_current_cpu() {
    match active_version() {
        GicVersion::V2 => gicv2::init_current_cpu(),
        GicVersion::V3 => gicv3::init_current_cpu(),
    }
    // Per‑CPU: this CPU's GIC interface is now ready for PMR access.
    karch::pmr::mark_ready();
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

/// Open the pseudo‑NMI window after acknowledging a non‑PMU interrupt.
///
/// Sets PMR to [`pmr::NMI_ONLY`] so only pseudo‑NMIs can preempt the handler,
/// then clears `DAIF.I` so those NMIs are actually delivered.  Normal IRQs
/// remain masked by PMR until [`close_nmi_window`] is called.
///
/// # Pairing invariant
///
/// Every call to `open_nmi_window()` **must** be matched by exactly one call
/// to [`close_nmi_window()`] on the same CPU and on every path (including
/// error / unwind).  The pairing is maintained by the `DispatchedIrq` guard:
///
/// ```text
///   dispatch_irq_by_gic_version()               // → open_nmi_window()
///   → irq_handler vector                         // handler runs
///   → dispatched_irq.complete()                  // → close_nmi_window()
///   // … or DispatchedIrq::drop() on unwind       // → close_nmi_window() (panic=unwind only)
/// ```
///
/// **Caveat:** the kernel uses `panic = "abort"`; a panic inside the handler
/// causes `shutdown()` via `#[panic_handler]` without running `Drop`.  The NMI
/// window "leaks" on the panicked CPU, but the CPU is halted immediately —
/// normal IRQs can never be delivered there anyway.
///
/// # Caller constraints
///
/// Must be called from the IRQ exception level with `DAIF.I` already set by
/// exception entry, and only after [`pmr::is_ready`] returns `true` on this
/// CPU.  Must **never** be called from NMI context (not re‑entrant).
#[cfg(feature = "nmi-pmu")]
fn open_nmi_window() {
    assert!(karch::pmr::is_ready());
    karch::pmr::write(karch::pmr::NMI_ONLY);
    // SAFETY: exception entry set DAIF.I; clear it so pseudo‑NMI can nest
    // while normal IRQs remain gated by PMR.
    //
    // These functions are NOT re‑entrant: the caller must ensure they are
    // never invoked from NMI context.
    unsafe { core::arch::asm!("msr daifclr, #2") };
}

/// Close the pseudo‑NMI window before deactivating the interrupt.
///
/// Re‑masks `DAIF.I` then restores PMR to [`pmr::ALL`] so normal IRQs are
/// unmasked again.  This is the symmetric counterpart to [`open_nmi_window`].
///
/// # Pairing invariant
///
/// Must be called exactly once per [`open_nmi_window()`] on the same CPU.
/// See [`open_nmi_window()`] for the full contract and panic‑abort caveat.
///
/// # Caller constraints
///
/// Must be called from the same IRQ exception context that called
/// `open_nmi_window()`, before the interrupt is deactivated (EOI / DIR),
/// and only after [`pmr::is_ready`] returns `true`.
/// Must **never** be called from NMI context.
#[cfg(feature = "nmi-pmu")]
fn close_nmi_window() {
    assert!(karch::pmr::is_ready());
    // SAFETY: restore the pre‑exception mask before deactivating the IRQ.
    unsafe { core::arch::asm!("msr daifset, #2") };
    karch::pmr::write(karch::pmr::ALL);
}

/// Dispatch the current hardware interrupt.
///
/// The `_unused` parameter exists only to satisfy the
/// [`khal::irq::IntrManagerIf::dispatch_irq`] trait signature, which passes an
/// IRQ identifier used by architectures like x86.
/// On GIC (both v2 and v3), the pending interrupt ID is read directly from the
/// hardware acknowledge register (IAR), so the parameter is never consumed.
/// It is intentionally kept unnamed to avoid a register move on this hot path;
/// callers cannot remove it without breaking the trait contract.
///
/// # NMI window (feature = `nmi-pmu`)
///
/// For non‑PMU interrupts this calls `open_nmi_window()`. The caller
/// **must** ensure the returned `completion_cookie` is eventually passed to
/// [`complete_irq()`] (or `DispatchedIrq::complete` / `Drop`) to invoke
/// `close_nmi_window()`. See `open_nmi_window()` for the full pairing
/// contract and the `panic = "abort"` caveat.
pub fn dispatch_irq_by_gic_version(_unused: usize) -> Option<(usize, usize)> {
    let res = match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq(),
        GicVersion::V3 => gicv3::dispatch_irq(),
    };
    #[cfg(feature = "nmi-pmu")]
    if let Some((irq, _)) = res
        && irq != of::pmu_irq_or(kbuild_config::PMU_IRQ)
    {
        open_nmi_window();
    }
    res
}

/// Complete (deactivate) a previously dispatched interrupt.
///
/// For non‑PMU interrupts this calls `close_nmi_window()` to restore PMR
/// and re‑mask `DAIF.I`, pairing with the `open_nmi_window()` call in
/// [`dispatch_irq_by_gic_version()`].
///
/// The `completion_cookie` is decoded per GIC version before comparing with
/// `PMU_IRQ`, matching the INTID‑based comparison in the dispatch path.
pub fn complete_irq(completion_cookie: usize) {
    let version = active_version();
    #[cfg(feature = "nmi-pmu")]
    {
        let is_pmu = match version {
            // GICv2: cookie is `u32::from(Ack)`, which for SGIs encodes
            // CPU ID alongside INTID.  Decode via Ack::from to extract
            // the INTID before comparing — keeping this consistent with
            // dispatch_irq_by_gic_version which also compares the INTID.
            GicVersion::V2 => {
                gicv2::intid_from_cookie(completion_cookie) as usize
                    == of::pmu_irq_or(kbuild_config::PMU_IRQ)
            }
            // GICv3: cookie is directly the INTID (dispatch returns (irq, irq)).
            GicVersion::V3 => completion_cookie == of::pmu_irq_or(kbuild_config::PMU_IRQ),
        };
        if !is_pmu {
            close_nmi_window();
        }
    }
    match version {
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

pub fn set_prio(irq: usize, priority: u8) {
    match active_version() {
        GicVersion::V2 => gicv2::set_prio(irq, priority),
        GicVersion::V3 => gicv3::set_prio(irq, priority),
    }
}

#[kplat::impl_dev_interface]
impl khal::irq::IntrManagerIf {
    fn configure(desc: khal::irq::IrqDesc) {
        match desc.trigger {
            khal::irq::IrqTrigger::EdgeRising | khal::irq::IrqTrigger::EdgeFalling => {
                crate::gic::set_trigger(desc.hwirq, true);
            }
            khal::irq::IrqTrigger::LevelHigh | khal::irq::IrqTrigger::LevelLow => {
                crate::gic::set_trigger(desc.hwirq, false);
            }
            khal::irq::IrqTrigger::Unknown(_) => {}
        }
    }

    fn enable(irq: usize, enabled: bool) {
        crate::gic::enable(irq, enabled);
    }

    fn dispatch_irq(irq: usize) -> Option<khal::irq::DispatchedIrq> {
        crate::gic::dispatch_irq_by_gic_version(irq).map(|(hwirq, completion_cookie)| {
            khal::irq::DispatchedIrq::new(
                khal::irq::resolve_hwirq(GIC_ROOT_DOMAIN, hwirq),
                completion_cookie,
            )
        })
    }

    fn complete_irq(completion_cookie: usize) {
        crate::gic::complete_irq(completion_cookie);
    }

    fn notify_cpu(interrupt_id: usize, target: khal::irq::TargetCpu) {
        crate::gic::notify_cpu(interrupt_id, target);
    }

    fn set_prio(irq: usize, priority: u8) {
        crate::gic::set_prio(irq, priority);
    }
}
