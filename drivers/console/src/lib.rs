// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

use core::sync::atomic::{AtomicBool, Ordering};

use khal::{irq::IrqDesc, mem::PhysAddr};
use lazyinit::LazyInit;
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use memspace::iomap_device;

#[cfg(feature = "ns16550-ioport")]
mod ns16550_ioport;
#[cfg(feature = "ns16550-mmio")]
mod ns16550_mmio;
#[cfg(feature = "pl011")]
mod pl011;

#[cfg(all(
    feature = "pl011",
    any(feature = "ns16550-mmio", feature = "ns16550-ioport")
))]
compile_error!("console-driver expects exactly one backend feature");
#[cfg(all(feature = "ns16550-mmio", feature = "ns16550-ioport"))]
compile_error!("console-driver expects exactly one backend feature");
#[cfg(not(any(
    feature = "pl011",
    feature = "ns16550-mmio",
    feature = "ns16550-ioport"
)))]
compile_error!("console-driver requires one backend feature");

#[cfg(feature = "ns16550-ioport")]
use self::ns16550_ioport as backend;
#[cfg(feature = "ns16550-mmio")]
use self::ns16550_mmio as backend;
#[cfg(feature = "pl011")]
use self::pl011 as backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleTransport {
    Mmio { paddr: PhysAddr, size: usize },
    IoPort { io_port: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleSource {
    DeviceTree,
    Acpi,
    PlatformStatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsoleConfig {
    pub transport: ConsoleTransport,
    pub irq: Option<IrqDesc>,
    pub source: ConsoleSource,
}

impl ConsoleConfig {
    pub const fn mmio(
        paddr: PhysAddr,
        size: usize,
        irq: Option<IrqDesc>,
        source: ConsoleSource,
    ) -> Self {
        Self {
            transport: ConsoleTransport::Mmio { paddr, size },
            irq,
            source,
        }
    }

    pub const fn ioport(io_port: u16, irq: Option<IrqDesc>, source: ConsoleSource) -> Self {
        Self {
            transport: ConsoleTransport::IoPort { io_port },
            irq,
            source,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConsoleState {
    config: ConsoleConfig,
    irq: Option<IrqDesc>,
}

static CONSOLE: LazyInit<ConsoleState> = LazyInit::new();
static INPUT_IRQ_REGISTERED: AtomicBool = AtomicBool::new(false);

fn selected_config() -> Option<ConsoleConfig> {
    CONSOLE.get().map(|state| state.config)
}

fn selected_irq_desc() -> Option<IrqDesc> {
    CONSOLE.get().and_then(|state| state.irq)
}

pub fn config() -> Option<ConsoleConfig> {
    selected_config()
}

pub fn interrupt_id() -> Option<usize> {
    selected_irq_desc().and_then(IrqDesc::logical_irq)
}

#[cfg(feature = "ns16550-ioport")]
pub fn boot_console_io_port() -> u16 {
    0x3f8
}

pub fn config_from_device_tree() -> Option<ConsoleConfig> {
    let stdout_path = of::chosen_stdout_path()?;
    let node = of::resolve_node(stdout_path)?;
    let reg = node.reg()?.next()?;

    #[cfg(feature = "pl011")]
    if !node
        .compatibles()
        .any(|compatible| compatible == "arm,pl011")
    {
        return None;
    }

    #[cfg(feature = "ns16550-mmio")]
    if !node.compatibles().any(|compatible| {
        matches!(
            compatible,
            "ns16550a" | "ns16550" | "snps,dw-apb-uart" | "mrvl,mmp-uart"
        )
    }) {
        return None;
    }

    #[cfg(feature = "ns16550-ioport")]
    {
        let _ = node;
        let _ = reg;
        None
    }

    #[cfg(not(feature = "ns16550-ioport"))]
    {
        Some(ConsoleConfig::mmio(
            PhysAddr::from_usize(reg.starting_address as usize),
            reg.size,
            of::first_interrupt_desc(node).map(device_tree_irq_desc),
            ConsoleSource::DeviceTree,
        ))
    }
}

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
pub fn init_from_device_tree() -> Option<ConsoleConfig> {
    let config = config_from_device_tree()?;
    init(config);
    Some(config)
}

#[cfg(feature = "ns16550-ioport")]
pub fn init_from_device_tree() -> Option<ConsoleConfig> {
    None
}

#[cfg(not(feature = "ns16550-ioport"))]
fn device_tree_irq_desc(info: of::InterruptInfo) -> IrqDesc {
    let desc = match info.controller {
        of::InterruptControllerKind::Gic => match info.trigger {
            of::InterruptTrigger::EdgeRising => khal::irq::gic_edge_irq_desc(info.irq),
            of::InterruptTrigger::EdgeFalling => {
                khal::irq::gic_irq_desc(info.irq, khal::irq::IrqTrigger::EdgeFalling)
            }
            of::InterruptTrigger::LevelHigh => khal::irq::gic_level_irq_desc(info.irq),
            of::InterruptTrigger::LevelLow => {
                khal::irq::gic_irq_desc(info.irq, khal::irq::IrqTrigger::LevelLow)
            }
            of::InterruptTrigger::Unknown(flags) => {
                khal::irq::gic_irq_desc(info.irq, khal::irq::IrqTrigger::Unknown(flags))
            }
        },
        of::InterruptControllerKind::Plic => khal::irq::plic_irq_desc(info.irq),
        of::InterruptControllerKind::Unknown => khal::irq::IrqDesc::from_hwirq(info.irq),
    };

    desc.with_source(khal::irq::IrqSource::DeviceTree)
}

#[cfg(feature = "pl011")]
fn init_backend(config: ConsoleConfig) {
    match config.transport {
        ConsoleTransport::Mmio { paddr, size } => {
            let uart_base = iomap_device(paddr, size, "console-pl011")
                .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"));
            backend::init(uart_base);
        }
        ConsoleTransport::IoPort { .. } => panic!("pl011 does not support ioport transport"),
    }
}

#[cfg(feature = "ns16550-mmio")]
fn init_backend(config: ConsoleConfig) {
    match config.transport {
        ConsoleTransport::Mmio { paddr, size } => {
            let uart_base = iomap_device(paddr, size, "console-ns16550")
                .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"));
            backend::init(uart_base);
        }
        ConsoleTransport::IoPort { .. } => panic!("ns16550-mmio does not support ioport transport"),
    }
}

#[cfg(feature = "ns16550-ioport")]
fn init_backend(config: ConsoleConfig) {
    match config.transport {
        ConsoleTransport::Mmio { .. } => panic!("ns16550-ioport does not support mmio transport"),
        ConsoleTransport::IoPort { io_port } => backend::init(io_port),
    }
}

fn remember_config(config: ConsoleConfig) {
    let irq = config.irq.map(|desc| desc.with_virq(khal::irq::map(desc)));
    INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
    CONSOLE.init_once(ConsoleState { config, irq });
}

pub fn init(config: ConsoleConfig) {
    remember_config(config);
    init_backend(config);
}

pub fn write_data(bytes: &[u8]) {
    let _ = selected_config().expect("console config not initialized");
    backend::write_data(bytes);
}

pub fn read_data(bytes: &mut [u8]) -> usize {
    let _ = selected_config().expect("console config not initialized");
    backend::read_data(bytes)
}

pub fn getchar() -> Option<u8> {
    let _ = selected_config().expect("console config not initialized");
    backend::getchar()
}

fn handle_input_irq() {
    backend::ack_interrupt();
}

pub fn register_input_irq_handler() {
    let Some(desc) = selected_irq_desc() else {
        return;
    };
    if INPUT_IRQ_REGISTERED.load(Ordering::Acquire) {
        return;
    }
    if INPUT_IRQ_REGISTERED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    if !khal::irq::register(desc, handle_input_irq) {
        INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
        panic!("failed to register console input IRQ handler: {desc:?}");
    }
}
