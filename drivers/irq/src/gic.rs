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

/// Open the pseudo‑NMI window for the normal IRQ path.
///
/// Sets PMR to [`pmr::NMI_ONLY`] so only pseudo‑NMIs can preempt
/// the handler, then clears `DAIF.I` so those NMIs are delivered.
/// PMR is restored by the exception exit path (saved at entry).
///
/// Must be called from IRQ exception context only (never from NMI).
#[cfg(feature = "nmi-pmu")]
fn open_nmi_window() {
    assert!(karch::pmr::is_ready());
    karch::pmr::write(karch::pmr::NMI_ONLY);
    // SAFETY: exception entry set DAIF.I; clear it so pseudo‑NMI can nest
    // while normal IRQs remain gated by PMR.
    unsafe { core::arch::asm!("msr daifclr, #2") };
}

/// IRQ path: ack → RPR check → early EOI, version dispatch.
/// Returns (hwirq, cookie, is_nmi) for window decision.
pub fn gic_handle_irq_from_irqson(_unused: usize) -> Option<(usize, usize, bool)> {
    match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq_from_irqson(),
        GicVersion::V3 => gicv3::dispatch_irq_from_irqson(),
    }
}

/// NMI path: interrupt taken from a context with IRQs **disabled**.
///
/// Mirrors Linux `__gic_handle_irq_from_irqsoff`.  Dispatches to the
/// version‑specific irqsoff backend (which saves PMR, lowers to
/// NMI_ONLY before ack, then restores PMR).  Does **not** open the NMI
/// window.  Caller must eventually invoke [`complete_irq`].
pub fn gic_handle_irq_from_irqsoff(_unused: usize) -> Option<(usize, usize)> {
    match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq_from_irqsoff(),
        GicVersion::V3 => gicv3::dispatch_irq_from_irqsoff(),
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

    /// IRQ path: IRQs were enabled → may be IRQ or NMI.
    ///
    /// Uses GIC RPR to detect NMIs: if NMI, skip the NMI window
    /// (we ARE the NMI) and go through dispatch_subscribers (safe —
    /// no locks held in IRQ-enabled context).
    fn dispatch_irq(irq: usize) -> Option<khal::irq::PendingIrq> {
        let (hwirq, completion_cookie, is_nmi) = crate::gic::gic_handle_irq_from_irqson(irq)?;

        // Linux: open window only for non-NMI, after NMI is handled.
        if !is_nmi {
            #[cfg(feature = "nmi-pmu")]
            crate::gic::open_nmi_window();
        }

        Some(khal::irq::PendingIrq::new(
            khal::irq::IrqRef::Domain(GIC_ROOT_DOMAIN, hwirq),
            completion_cookie,
        ))
    }

    /// NMI path: IRQs were disabled → no lock, no NMI window, PMR protected.
    fn dispatch_nmi(irq: usize) -> Option<khal::irq::DispatchedIrq> {
        let (hwirq, completion_cookie) = crate::gic::gic_handle_irq_from_irqsoff(irq)?;
        Some(khal::irq::DispatchedIrq::new(hwirq, completion_cookie))
    }

    /// Deactivate the interrupt.  PMR is restored by exception exit.
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
