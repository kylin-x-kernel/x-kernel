// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Early-boot console: UART drivers plus console/emergency-output runtime.
//!
//! This crate owns the dedicated boot console path that runs before the
//! bus manager and driver registry exist. `serial` provides PL011 and
//! NS16550 (MMIO or IO-port) UART drivers; `runtime` tracks the runtime
//! identity of the adopted stdout port, without a console-selection API.
//!
//! # Example
//!
//! Serial integration publishes the adopted boot port through
//! [`runtime::register_stdout_console`]. The native TTY obtains its runtime
//! handle through [`runtime::stdout_console`] and keeps that connection fixed.
//! Normal and emergency kernel output use the early port through kiface-console-api contracts,
//! independently of runtime device publication.
//!
//! ```ignore
//! use kclass::CharDevice;
//! use console_driver::runtime;
//!
//! if let Some(console) = runtime::stdout_console() {
//!     console.write(b"[kinit] starting services\n")?;
//! }
//! ```
#![no_std]

extern crate alloc;

#[cfg(feature = "ns16550-mmio")]
mod ns16550_mmio;
pub mod runtime;
pub mod serial;

use alloc::sync::Arc;

#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
use khal::mem::PhysAddr;
use kirq::IrqActionToken;
#[cfg(any(all(feature = "ns16550-ioport", target_arch = "x86_64"), unittest))]
use kirq::IrqDesc;
use kspin::{SpinNoIrq, static_lock};
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
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Parsed stdout fields are consumed by different enabled \
              serial backends"
)]
#[allow(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: Parsed stdout fields are consumed by different \
              enabled serial backends"
)]
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
    irq: Option<serial::EarlyIrq>,
}

/// Device-tree MMIO layout for an NS16550-compatible UART.
#[cfg(feature = "ns16550-mmio")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SerialMmioLayout {
    /// Device-tree `reg-shift`: adjacent register offset is `1 << reg_shift`.
    pub reg_shift: u32,
    /// Device-tree `reg-io-width` decoded as a typed MMIO access width.
    pub reg_width: SerialRegWidth,
}

struct InputIrqState {
    has_attempted: bool,
    // Retain the line and action removal key; this boot-lifetime registration
    // has no teardown, and the token itself does not unregister on drop.
    registration: Option<(usize, IrqActionToken)>,
}

static_lock! {
    static INPUT_IRQ: SpinNoIrq<InputIrqState> = SpinNoIrq::new(InputIrqState {
        has_attempted: false,
        registration: None,
    });
}

/// Fixed I/O-port base for the NS16550 ISA COM port used as stdout on x86.
#[cfg(feature = "ns16550-ioport")]
pub fn boot_console_io_port() -> u16 {
    0x3f8
}

// Ordinary console I/O provider. `kiface` permits exactly one
// implementation per interface, so it lives here at the crate root and routes
// printk to the early stdout [`SerialPort`]. The stdout port is brought up in
// platform `early_driver_init`, before the driver model exists, so this is the
// one console path that must not depend on kdriver.
#[kiface::provide]
impl kiface_console_api::ConsoleIoIf {
    fn write_data(buf: &[u8]) {
        if let Some(port) = serial::stdout_port() {
            port.write_data(buf);
        }
    }

    fn read_data(buf: &mut [u8]) -> usize {
        serial::stdout_port()
            .map(|port| port.read_data(buf))
            .unwrap_or(0)
    }
}

/// Read one byte from the stdout console, if it has been brought up.
pub fn getchar() -> Option<u8> {
    serial::stdout_port().and_then(|port| port.getchar())
}

/// Resolve the backend-neutral firmware node for a `chosen` stdout path.
///
/// The path is resolved through `/aliases` first (the `of` and `fwnode` views
/// do not share handles), then walked by node name from the firmware root.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn stdout_fwnode(stdout_path: &str) -> Option<fwnode::FwNode<'static>> {
    let key = stdout_path.split(':').next()?;
    let path = if key.starts_with('/') {
        key
    } else {
        of::fdt()?
            .find_node("/aliases")?
            .property_str(key)?
            .split(':')
            .next()?
    };
    let mut node = khal::firmware::fwnode_root()?;
    for component in path.split('/').filter(|component| !component.is_empty()) {
        let mut child = node.first_child()?;
        loop {
            if child.name() == Some(component) {
                node = child;
                break;
            }
            child = child.next_sibling()?;
        }
    }
    Some(node)
}

/// The stdout node's IRQ declaration as firmware cells plus controller node.
///
/// `Ok(None)` means the node declares no interrupt; an invalid declaration is
/// an error and must not be treated as absence.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn stdout_firmware_irq(
    stdout_path: &str,
) -> Result<Option<fwnode::FwIrqSpec<'static>>, fwnode::FwError> {
    let Some(node) = stdout_fwnode(stdout_path) else {
        return Ok(None);
    };
    node.irq_spec(0)
}

/// Resolve the device-tree stdout node into a port description.
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn stdout_desc_from_device_tree() -> Option<StdoutDesc> {
    let stdout_path = of::chosen_stdout_path()?;
    let node = of::resolve_node(stdout_path)?;
    let reg = node.reg()?.next()?;
    let paddr = PhysAddr::from_usize(reg.starting_address as usize);
    let irq = match stdout_firmware_irq(stdout_path) {
        Ok(Some(spec)) => Some(serial::EarlyIrq::Firmware(spec)),
        Ok(None) => None,
        Err(error) => {
            // Keep polling output, but do not re-decode through the legacy OF
            // path: a declaration that exists but is invalid is not absence.
            log::warn!("console: invalid stdout IRQ declaration; input stays polled: {error:?}");
            None
        }
    };
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

/// Map the stdout MMIO window and build the matching [`SerialPort`].
#[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
fn build_stdout_port(desc: &StdoutDesc) -> SerialPort {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the firmware-described stdout \
                  UART window cannot be mapped; console initialization has no error return or \
                  recovery"
    )]
    let uart_base = memspace::iomap_device(desc.paddr, desc.size, "console")
        .unwrap_or_else(|err| panic!("failed to iomap console: {err:?}"));
    match desc.kind {
        #[cfg(feature = "pl011")]
        StdoutKind::Pl011 => {
            // SAFETY: iomap_device mapped the PL011 window described by the
            // stdout node. The mapping persists and setup precedes publication.
            unsafe {
                SerialPort::new_mmio_pl011(uart_base, desc.paddr, desc.size, SerialRole::Stdout)
            }
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
    serial::register_early_stdout(Arc::new(port), desc.irq);
    Some(())
}

/// Bring up a fixed I/O-port stdout console (x86 ISA COM).
#[cfg(all(feature = "ns16550-ioport", target_arch = "x86_64"))]
pub fn init_stdout_ioport(port: u16, irq: Option<IrqDesc>) {
    // SAFETY: `port` is the platform-configured NS16550 I/O-port base.
    let uart = unsafe { SerialPort::new_ioport_ns16550(port, SerialRole::Stdout) };
    serial::register_early_stdout(Arc::new(uart), irq.map(serial::EarlyIrq::Legacy));
}

// Keep failure tests off the live console/IRQ registry. Hardware activation
// and readiness publication remain in this shared production transaction.
fn prepare_input_with(
    input_irq: &SpinNoIrq<InputIrqState>,
    console: Option<(Arc<SerialPort>, serial::EarlyIrq)>,
    prepare_fn: impl FnOnce(&SerialPort) -> Result<(), alloc::collections::TryReserveError>,
    map_fn: impl FnOnce(serial::EarlyIrq) -> Option<usize>,
    register_fn: impl FnOnce(usize, kirq::Handler) -> Result<Option<IrqActionToken>, kirq::IrqDescError>,
) {
    {
        let mut state = input_irq.lock();
        if state.has_attempted {
            return;
        }
        state.has_attempted = true;
    }
    let Some((port, desc)) = console else {
        return;
    };
    if let Err(error) = prepare_fn(&port) {
        log::warn!("console input stays polled: buffer preparation failed: {error:?}");
        return;
    }
    let Some(virq) = map_fn(desc) else {
        log::warn!("console input stays polled: cannot map {desc:?}");
        return;
    };
    let irq_port = port.clone();
    // A mapping may predate this attempt or serve another shared action.
    // On rejection retain it and the input buffer, without unmasking this UART.
    match register_fn(virq, Arc::new(move |_| irq_port.handle_input_irq())) {
        Ok(Some(token)) => {
            port.enable_receive_interrupt();
            input_irq.lock().registration = Some((virq, token));
        }
        Ok(None) => {
            log::warn!("console input stays polled: action rejected for {desc:?}");
        }
        Err(err) => {
            log::warn!("console input stays polled: action failed for {desc:?}: {err:?}");
        }
    }
}

/// Map the early console's interrupt description through the same IRQ core
/// paths used by other consumers: firmware declarations by exact controller
/// node, legacy descriptions by their pre-decoded identity.
fn map_console_irq(irq: serial::EarlyIrq) -> Option<usize> {
    match irq {
        #[cfg(any(feature = "pl011", feature = "ns16550-mmio"))]
        serial::EarlyIrq::Firmware(spec) => match kirq::try_map_fwspec(spec) {
            Ok(mapped) => Some(mapped.virq),
            Err(error) => {
                log::warn!("console input stays polled: firmware IRQ mapping failed: {error:?}");
                None
            }
        },
        #[cfg(any(all(feature = "ns16550-ioport", target_arch = "x86_64"), unittest))]
        serial::EarlyIrq::Legacy(desc) => match kirq::try_map(desc) {
            Ok(virq) => Some(virq),
            Err(error) => {
                log::warn!("console input stays polled: cannot map {desc:?}: {error:?}");
                None
            }
        },
    }
}

unittest::path_mod_test!(input_setup_tests, "../ut-tests/input_setup.rs");

#[kiface::provide]
impl kiface_console_api::EmergencyConsoleWriteIf {
    fn write_data_atomic(buf: &[u8]) {
        if let Some(port) = serial::stdout_port() {
            port.write_data_atomic(buf);
        }
    }
}

#[kiface::provide]
impl kiface_console_api::ConsoleInputSetupIf {
    fn interrupt_id() -> Option<usize> {
        INPUT_IRQ
            .lock()
            .registration
            .as_ref()
            .map(|(virq, _)| *virq)
    }

    fn prepare_input() {
        prepare_input_with(
            &INPUT_IRQ,
            serial::stdout_port().zip(serial::stdout_irq()),
            SerialPort::prepare_irq_input,
            map_console_irq,
            kirq::try_register_shared,
        );
    }
}
