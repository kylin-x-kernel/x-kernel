// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;

#[cfg(feature = "ns16550-mmio")]
mod ns16550_mmio;
pub mod runtime;
pub mod serial;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use khal::mem::PhysAddr;
use kirq::{IrqActionToken, IrqDesc, IrqEvent};
use kspin::SpinNoIrq;
#[cfg(feature = "ns16550-mmio")]
pub use ns16550_mmio::SerialRegWidth;
pub use serial::{SerialIdent, SerialPort, SerialRole};

#[cfg(not(any(
    feature = "pl011",
    feature = "ns16550-mmio",
    feature = "ns16550-ioport"
)))]
compile_error!(
    "console-driver requires at least one backend feature: `pl011`, `ns16550-mmio`, or \
     `ns16550-ioport`"
);

/// Which UART family the device-tree stdout node is.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
enum StdoutKind {
    #[cfg(feature = "pl011")]
    Pl011,
    #[cfg(feature = "ns16550-mmio")]
    Ns16550Mmio,
}

/// Parsed device-tree stdout node: just enough to build a [`SerialPort`] and
/// wire its input interrupt.
#[allow(dead_code)]
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
struct StdoutDesc {
    kind: StdoutKind,
    paddr: PhysAddr,
    size: usize,
    /// 16550 register stride as `1 << reg-shift` (each register is that many
    /// bytes apart). The RK3588 DesignWare UART uses `reg-shift = 2` (4-byte
    /// stride). Ignored by the PL011 backend.
    reg_shift: u32,
    /// 16550 MMIO register access width from `reg-io-width`.
    #[cfg(feature = "ns16550-mmio")]
    reg_width: SerialRegWidth,
    irq: Option<IrqDesc>,
}

/// Device-tree MMIO layout for an NS16550-compatible UART.
#[cfg(feature = "ns16550-mmio")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SerialMmioLayout {
    /// Device-tree `reg-shift`: adjacent register offset is `1 << reg_shift`.
    pub reg_shift: u32,
    /// Device-tree `reg-io-width` decoded as a typed MMIO access width.
    pub reg_width: SerialRegWidth,
}

static INPUT_IRQ_REGISTERED: AtomicBool = AtomicBool::new(false);
/// Keeps the console input interrupt registration identity for diagnostics.
static INPUT_IRQ: SpinNoIrq<Option<IrqActionToken>> = SpinNoIrq::new(None);

/// Fixed I/O-port base for the NS16550 ISA COM port used as stdout on x86.
#[cfg(feature = "ns16550-ioport")]
pub fn boot_console_io_port() -> u16 {
    0x3f8
}

// The single `ConsoleIf` implementation. `kiface` permits exactly one
// implementation per interface, so it lives here at the crate root and routes
// printk to the early stdout [`SerialPort`]. The stdout port is brought up in
// platform `early_driver_init`, before the driver model exists, so this is the
// one console path that must not depend on kdriver.
#[kplat::impl_dev_interface]
impl khal::console::ConsoleIf {
    fn write_data(buf: &[u8]) {
        if let Some(port) = serial::stdout_port() {
            port.write_data(buf);
        }
    }

    fn write_data_atomic(buf: &[u8]) {
        if let Some(port) = serial::stdout_port() {
            port.write_data(buf);
        }
    }

    fn read_data(buf: &mut [u8]) -> usize {
        serial::stdout_port()
            .map(|port| port.read_data(buf))
            .unwrap_or(0)
    }

    fn interrupt_id() -> Option<usize> {
        serial::stdout_irq().and_then(IrqDesc::logical_irq)
    }
}

/// Read one byte from the stdout console, if it has been brought up.
pub fn getchar() -> Option<u8> {
    serial::stdout_port().and_then(|port| port.getchar())
}

/// Resolve the device-tree stdout node into a port description.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn stdout_desc_from_device_tree() -> Option<StdoutDesc> {
    let stdout_path = of::chosen_stdout_path()?;
    let node = of::resolve_node(stdout_path)?;
    let reg = node.reg()?.next()?;
    let paddr = PhysAddr::from_usize(reg.starting_address as usize);
    let irq = of::first_interrupt_desc(node).map(device_tree_irq_desc);
    // Register stride: each UART register is `1 << reg-shift` bytes apart
    // (RK3588's DesignWare UART uses reg-shift = 2 -> 4-byte stride). PL011
    // does not use this property; it is recorded but ignored by that backend.
    let reg_shift = node.property_u32("reg-shift").unwrap_or(0);
    #[cfg(feature = "ns16550-mmio")]
    let reg_width = serial_reg_width_from_node(node)?;

    #[cfg(feature = "pl011")]
    if node
        .compatibles()
        .any(|compatible| compatible == "arm,pl011")
    {
        return Some(StdoutDesc {
            kind: StdoutKind::Pl011,
            paddr,
            size: reg.size,
            reg_shift,
            #[cfg(feature = "ns16550-mmio")]
            reg_width,
            irq,
        });
    }

    #[cfg(feature = "ns16550-mmio")]
    if node.compatibles().any(|compatible| {
        matches!(
            compatible,
            "ns16550a" | "ns16550" | "snps,dw-apb-uart" | "mrvl,mmp-uart"
        )
    }) {
        return Some(StdoutDesc {
            kind: StdoutKind::Ns16550Mmio,
            paddr,
            size: reg.size,
            reg_shift,
            #[cfg(feature = "ns16550-mmio")]
            reg_width,
            irq,
        });
    }

    None
}

#[cfg(feature = "ns16550-mmio")]
fn serial_reg_width_from_node(node: of::FdtNode<'static, 'static>) -> Option<SerialRegWidth> {
    SerialRegWidth::from_bytes(node.property_u32("reg-io-width").unwrap_or(1))
}

/// Read the NS16550 MMIO DT layout for the node whose MMIO base matches
/// `paddr`. Returns 8-bit, unshifted access when the node is not found, matching
/// the Linux 8250 default for absent firmware properties.
///
/// Used by the runtime serial driver to honor `reg-shift` and `reg-io-width`
/// for auxiliary (non-stdout) 16550 ports that were not covered by the
/// early-boot DT parse.
#[cfg(feature = "ns16550-mmio")]
pub fn ns16550_mmio_layout_for_paddr(paddr: usize) -> Option<SerialMmioLayout> {
    let Some(fdt) = of::fdt() else {
        return Some(SerialMmioLayout {
            reg_shift: 0,
            reg_width: SerialRegWidth::U8,
        });
    };
    for node in fdt.all_nodes() {
        if !(node.is_compatible("snps,dw-apb-uart")
            || node.is_compatible("ns16550a")
            || node.is_compatible("ns16550")
            || node.is_compatible("mrvl,mmp-uart"))
        {
            continue;
        }
        if let Some(reg) = node.reg() {
            for entry in reg {
                if entry.starting_address as usize == paddr {
                    return Some(SerialMmioLayout {
                        reg_shift: node.property_u32("reg-shift").unwrap_or(0),
                        reg_width: serial_reg_width_from_node(node)?,
                    });
                }
            }
        }
    }
    Some(SerialMmioLayout {
        reg_shift: 0,
        reg_width: SerialRegWidth::U8,
    })
}

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn device_tree_irq_desc(info: of::InterruptInfo) -> IrqDesc {
    let desc = match info.controller {
        of::InterruptControllerKind::Gic => match info.trigger {
            of::InterruptTrigger::EdgeRising => kirq::gic_edge_irq_desc(info.irq),
            of::InterruptTrigger::EdgeFalling => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::EdgeFalling)
            }
            of::InterruptTrigger::LevelHigh => kirq::gic_level_irq_desc(info.irq),
            of::InterruptTrigger::LevelLow => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::LevelLow)
            }
            of::InterruptTrigger::Unknown(flags) => {
                kirq::gic_irq_desc(info.irq, kirq::IrqTrigger::Unknown(flags))
            }
        },
        of::InterruptControllerKind::Plic => kirq::plic_irq_desc(info.irq),
        of::InterruptControllerKind::Unknown => kirq::IrqDesc::from_hwirq(info.irq),
    };

    desc.with_source(kirq::IrqSource::DeviceTree)
}

/// Map the stdout MMIO window and build the matching [`SerialPort`].
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn build_stdout_port(desc: &StdoutDesc) -> SerialPort {
    let uart_base = memspace::iomap_device(desc.paddr, desc.size, "console")
        .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"));
    match desc.kind {
        #[cfg(feature = "pl011")]
        StdoutKind::Pl011 => {
            SerialPort::new_mmio_pl011(uart_base, desc.paddr, desc.size, SerialRole::Stdout)
        }
        #[cfg(feature = "ns16550-mmio")]
        StdoutKind::Ns16550Mmio => {
            // SAFETY: `uart_base` is the exclusively-mapped NS16550 MMIO
            // window for this stdout node, returned by `memspace::iomap_device`.
            unsafe {
                SerialPort::new_mmio_ns16550(
                    uart_base,
                    desc.paddr,
                    desc.size,
                    SerialRole::Stdout,
                    desc.reg_shift,
                    desc.reg_width,
                )
            }
        }
    }
}

fn map_console_irq(desc: IrqDesc) -> Option<IrqDesc> {
    match kirq::try_map(desc) {
        Ok(virq) => Some(desc.with_virq(virq)),
        Err(err) => {
            log::warn!("failed to map console IRQ {desc:?}: {err:?}");
            None
        }
    }
}

/// Bring up the stdout console from the device tree.
///
/// Parses the `chosen` stdout node, builds the matching [`SerialPort`], and
/// registers it as the early stdout so printk and the runtime serial driver
/// share one instance. Returns `None` when the device tree does not describe a
/// console any enabled backend recognizes.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
pub fn init_stdout_from_device_tree() -> Option<()> {
    let desc = stdout_desc_from_device_tree()?;
    let port = build_stdout_port(&desc);
    let irq = desc.irq.and_then(map_console_irq);
    serial::register_early_stdout(Arc::new(port), irq);
    Some(())
}

/// Bring up a fixed I/O-port stdout console (x86 ISA COM).
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
pub fn init_stdout_ioport(port: u16, irq: Option<IrqDesc>) {
    // SAFETY: `port` is the platform-configured NS16550 I/O-port base.
    let uart = unsafe { SerialPort::new_ioport_ns16550(port, SerialRole::Stdout) };
    let irq = irq.and_then(map_console_irq);
    serial::register_early_stdout(Arc::new(uart), irq);
}

fn handle_input_irq(_irq: usize) -> IrqEvent {
    if let Some(port) = serial::stdout_port() {
        port.ack_interrupt();
    }
    IrqEvent::HANDLED
}

/// Register the stdout console's input interrupt handler.
///
/// Must be called after the platform IRQ provider is up; platforms differ on
/// whether that is `early_driver_init` or `final_init`.
pub fn register_input_irq_handler() {
    let Some(desc) = serial::stdout_irq() else {
        return;
    };
    let Some(virq) = desc.logical_irq() else {
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
    match kirq::try_register_shared(virq, Arc::new(handle_input_irq)) {
        Ok(Some(token)) => *INPUT_IRQ.lock() = Some(token),
        Ok(None) => {
            INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
            panic!("failed to register console input IRQ handler: {desc:?}");
        }
        Err(err) => {
            INPUT_IRQ_REGISTERED.store(false, Ordering::Release);
            panic!("failed to register console input IRQ handler: {desc:?} ({err:?})");
        }
    }
}

#[cfg(all(unittest, any(feature = "pl011", feature = "ns16550-mmio")))]
mod tests_irq_desc {
    use kirq::IrqSource;
    use unittest::{assert_eq, def_test};

    use super::*;

    /// `device_tree_irq_desc` must tag every parsed interrupt as device-tree
    /// sourced and propagate the controller-local IRQ number. (It deliberately
    /// does not over-assert the trigger/polarity encoding, which is owned by
    /// the `kirq` constructors.)
    #[def_test]
    fn device_tree_irq_desc_tags_source_and_propagates_hwirq() {
        let gic = of::InterruptInfo {
            irq: 33,
            trigger: of::InterruptTrigger::EdgeRising,
            controller: of::InterruptControllerKind::Gic,
        };
        let desc = device_tree_irq_desc(gic);
        assert_eq!(desc.source, IrqSource::DeviceTree);
        assert_eq!(desc.hwirq, 33);

        let plic = of::InterruptInfo {
            irq: 16,
            trigger: of::InterruptTrigger::LevelHigh,
            controller: of::InterruptControllerKind::Plic,
        };
        let desc = device_tree_irq_desc(plic);
        assert_eq!(desc.source, IrqSource::DeviceTree);
        assert_eq!(desc.hwirq, 16);
    }
}
