// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GIC interrupt controller integration for AArch64 platforms.
//!
//! DT-based platforms should use [`init_from_device_tree()`], which discovers
//! the primary GIC node, maps the required register frames through runtime
//! `iomap`, and dispatches to the matching GICv2/GICv3 backend. Platforms with
//! broken or incomplete firmware description can instead call [`init()`] with a
//! static [`GicConfig`].

mod gicv2;
mod gicv3;

use kplat::interrupts::TargetCpu;
use lazyinit::LazyInit;
use memaddr::{PhysAddr, VirtAddr};
use memspace::iomap_device;
use of::FdtNode;

const GICD_NAME: &str = "gicd";
const GICC_NAME: &str = "gicc";
const GICR_NAME: &str = "gicr";

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

#[derive(Debug, Clone, Copy)]
struct GicLayout {
    version: GicVersion,
    gicd: VirtAddr,
    cpu_if: VirtAddr,
}

static GIC_CONFIG: LazyInit<GicConfig> = LazyInit::new();
static ACTIVE_VERSION: LazyInit<GicVersion> = LazyInit::new();

const GIC_V2_COMPATIBLES: &[&str] = &["arm,gic-400", "arm,cortex-a15-gic", "arm,cortex-a9-gic"];

fn remember_version(version: GicVersion) {
    if let Some(active) = ACTIVE_VERSION.get() {
        assert_eq!(
            *active, version,
            "GIC backend already initialized with a different version"
        );
    } else {
        ACTIVE_VERSION.init_once(version);
    }
}

fn active_version() -> GicVersion {
    *ACTIVE_VERSION.get().expect("GIC not initialized")
}

fn remember_config(config: GicConfig) {
    if let Some(saved) = GIC_CONFIG.get() {
        assert_eq!(*saved, config, "GIC config changed after initialization");
    } else {
        GIC_CONFIG.init_once(config);
    }
    remember_version(config.version);
}

pub fn set_config(config: GicConfig) {
    remember_config(config);
}

fn find_primary_gic_node() -> Option<(FdtNode<'static, 'static>, GicVersion)> {
    let fdt = of::fdt()?;
    fdt.all_nodes().find_map(|node| {
        node.property("interrupt-controller")?;
        if node.is_compatible("arm,gic-v3") {
            return Some((node, GicVersion::V3));
        }
        if GIC_V2_COMPATIBLES
            .iter()
            .any(|compatible| node.is_compatible(compatible))
        {
            return Some((node, GicVersion::V2));
        }
        None
    })
}

fn read_reg_pair(
    node: FdtNode<'static, 'static>,
    what: &str,
) -> (PhysAddr, usize, PhysAddr, usize) {
    let mut regs = node
        .reg()
        .unwrap_or_else(|| panic!("GIC DT node missing reg property for {what}"));
    let reg0 = regs
        .next()
        .unwrap_or_else(|| panic!("GIC DT node missing first reg range for {what}"));
    let reg1 = regs
        .next()
        .unwrap_or_else(|| panic!("GIC DT node missing second reg range for {what}"));
    (
        PhysAddr::from_usize(reg0.starting_address as usize),
        reg0.size,
        PhysAddr::from_usize(reg1.starting_address as usize),
        reg1.size,
    )
}

fn config_from_device_tree() -> GicConfig {
    let (node, version) = find_primary_gic_node()
        .unwrap_or_else(|| panic!("failed to find a supported GIC interrupt-controller node"));

    match version {
        GicVersion::V2 => {
            let (gicd_paddr, gicd_size, gicc_paddr, gicc_size) = read_reg_pair(node, "GICv2");
            GicConfig {
                version,
                gicd: GicMmioRegion {
                    paddr: gicd_paddr,
                    size: gicd_size,
                },
                gicc: Some(GicMmioRegion {
                    paddr: gicc_paddr,
                    size: gicc_size,
                }),
                gicr: None,
            }
        }
        GicVersion::V3 => {
            let (gicd_paddr, gicd_size, gicr_paddr, gicr_size) = read_reg_pair(node, "GICv3");
            GicConfig {
                version,
                gicd: GicMmioRegion {
                    paddr: gicd_paddr,
                    size: gicd_size,
                },
                gicc: None,
                gicr: Some(GicMmioRegion {
                    paddr: gicr_paddr,
                    size: gicr_size,
                }),
            }
        }
    }
}

fn map_region(region: GicMmioRegion, name: &'static str) -> VirtAddr {
    iomap_device(region.paddr, region.size, name)
        .unwrap_or_else(|err| panic!("failed to iomap {name}: {err:?}"))
}

fn map_layout(config: GicConfig) -> GicLayout {
    match config.version {
        GicVersion::V2 => GicLayout {
            version: config.version,
            gicd: map_region(config.gicd, GICD_NAME),
            cpu_if: map_region(
                config.gicc.expect("missing GICC region for GICv2"),
                GICC_NAME,
            ),
        },
        GicVersion::V3 => GicLayout {
            version: config.version,
            gicd: map_region(config.gicd, GICD_NAME),
            cpu_if: map_region(
                config.gicr.expect("missing GICR region for GICv3"),
                GICR_NAME,
            ),
        },
    }
}

fn init_current_cpu_for(version: GicVersion) {
    match version {
        GicVersion::V2 => gicv2::init_current_cpu(),
        GicVersion::V3 => gicv3::init_current_cpu(),
    }
}

/// Initialize the already-selected GIC CPU interface for the current core.
pub fn init_current_cpu() {
    init_current_cpu_for(active_version());
}

/// Initialize the primary GIC instance discovered from the current device tree.
///
/// This entrypoint is intended for DT-driven AArch64 platforms such as qemu
/// virt. It maps the controller register frames through the runtime iomap
/// window and initializes the current CPU interface.
pub fn init_from_device_tree() {
    let config = config_from_device_tree();
    init(config);
}

/// Initialize the GIC using a statically supplied config.
pub fn init(config: GicConfig) {
    remember_config(config);
    let layout = map_layout(config);
    match layout.version {
        GicVersion::V2 => gicv2::init_global(layout.gicd, layout.cpu_if),
        GicVersion::V3 => gicv3::init_global(layout.gicd, layout.cpu_if),
    }
    init_current_cpu_for(layout.version);
}

/// Initialize a legacy statically-described GICv2 distributor + CPU interface.
pub fn init_gic(gicd_base: VirtAddr, gicc_base: VirtAddr) {
    remember_version(GicVersion::V2);
    gicv2::init_global(gicd_base, gicc_base);
    gicv2::init_current_cpu();
}

/// Initialize the GICv2 CPU interface for the current core.
pub fn init_gicc() {
    remember_version(GicVersion::V2);
    gicv2::init_current_cpu();
}

/// Initialize the GICv3 CPU interface for the current core.
pub fn init_gicr() {
    remember_version(GicVersion::V3);
    gicv3::init_current_cpu();
}

/// Configure the trigger type for an interrupt line.
pub fn set_trigger(interrupt_id: usize, edge: bool) {
    match active_version() {
        GicVersion::V2 => gicv2::set_trigger(interrupt_id, edge),
        GicVersion::V3 => gicv3::set_trigger(interrupt_id, edge),
    }
}

/// Enable or disable a GIC interrupt.
pub fn enable(irq: usize, enabled: bool) {
    match active_version() {
        GicVersion::V2 => gicv2::enable(irq, enabled),
        GicVersion::V3 => gicv3::enable(irq, enabled),
    }
}

/// Register an IRQ handler and enable the line if successful.
pub fn register_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
    match active_version() {
        GicVersion::V2 => gicv2::register_handler(irq, handler),
        GicVersion::V3 => gicv3::register_handler(irq, handler),
    }
}

/// Unregister an IRQ handler and disable the line.
pub fn unregister_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
    match active_version() {
        GicVersion::V2 => gicv2::unregister_handler(irq),
        GicVersion::V3 => gicv3::unregister_handler(irq),
    }
}

/// Set the priority for an interrupt line.
pub fn set_prio(irq: usize, priority: u8) {
    match active_version() {
        GicVersion::V2 => gicv2::set_prio(irq, priority),
        GicVersion::V3 => gicv3::set_prio(irq, priority),
    }
}

/// Dispatch an IRQ and return the acknowledged IRQ number.
pub fn dispatch_irq_irq(unused: usize, pmu_irq: usize) -> Option<usize> {
    match active_version() {
        GicVersion::V2 => gicv2::dispatch_irq(unused, pmu_irq),
        GicVersion::V3 => gicv3::dispatch_irq(unused, pmu_irq),
    }
}

/// Send a software interrupt to a target CPU.
pub fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match active_version() {
        GicVersion::V2 => gicv2::notify_cpu(interrupt_id, target),
        GicVersion::V3 => gicv3::notify_cpu(interrupt_id, target),
    }
}

/// Implement `kplat::interrupts::IntrManager` using this GIC backend.
#[allow(clippy::crate_in_macro_def)]
#[macro_export]
macro_rules! irq_if_impl {
    ($name:ident) => {
        struct $name;
        #[impl_dev_interface]
        impl kplat::interrupts::IntrManager for $name {
            fn enable(irq: usize, enabled: bool) {
                $crate::gic::enable(irq, enabled);
            }

            fn reg_handler(irq: usize, handler: kplat::interrupts::Handler) -> bool {
                $crate::gic::register_handler(irq, handler)
            }

            fn unreg_handler(irq: usize) -> Option<kplat::interrupts::Handler> {
                $crate::gic::unregister_handler(irq)
            }

            fn dispatch_irq(irq: usize) -> Option<usize> {
                let pmu_irq = kbuild_config::PMU_IRQ;
                $crate::gic::dispatch_irq_irq(irq, pmu_irq)
            }

            fn notify_cpu(interrupt_id: usize, target: kplat::interrupts::TargetCpu) {
                $crate::gic::notify_cpu(interrupt_id, target);
            }

            fn set_prio(irq: usize, priority: u8) {
                $crate::gic::set_prio(irq, priority);
            }
        }
    };
}
