// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-instance UART serial driver.
//!
//! Each UART node in the device tree becomes its own character device. The
//! stdout UART is already running from early boot, so it is adopted as-is (the
//! same instance printk uses — no remap); any additional UARTs are mapped and
//! initialized fresh here.
//!
//! One driver per UART family, because the platform-bus match collapses a
//! node's compatible strings to a single alias, so by the time `probe_device`
//! runs the concrete backend is only known from which driver matched.

use alloc::{boxed::Box, sync::Arc};

use char_driver::CharDevice;
use console_driver::{
    SerialIdent, SerialRole, runtime,
    serial::{SerialPort, take_early_port},
};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kclass::publish_char;
use kdevice::{BusTypeId, DeviceDriver, DeviceMatcher, DeviceObject, FirmwareMatchSpec};
#[cfg(any(feature = "serial-pl011", feature = "serial-ns16550-mmio"))]
use khal::mem::{PhysAddr, VirtAddr};

use crate::driver_registry::BoxedDriver;

/// Character-device wrapper around one per-instance UART port.
struct SerialCharDev(Arc<SerialPort>);

impl Device for SerialCharDev {
    fn name(&self) -> &str {
        "serial"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn irq(&self) -> Option<usize> {
        None
    }
}

impl CharDevice for SerialCharDev {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        Ok(self.0.read_data(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.0.write_data(buf);
        Ok(buf.len())
    }
}

/// Which UART family a probed node belongs to.
#[cfg(any(feature = "serial-pl011", feature = "serial-ns16550-mmio"))]
enum SerialKind {
    #[cfg(feature = "serial-pl011")]
    Pl011,
    #[cfg(feature = "serial-ns16550-mmio")]
    Ns16550Mmio,
}

/// Resolve the port for a probed device.
///
/// Adopts the early stdout instance when this node is the console UART, so the
/// runtime device shares the hardware printk already uses; otherwise maps and
/// builds a fresh per-instance port tied to the device. The returned port's
/// [`SerialRole`] encodes which case applied, so callers can dispatch on it.
#[cfg(any(feature = "serial-pl011", feature = "serial-ns16550-mmio"))]
fn resolve_port(device: &DeviceObject, kind: SerialKind) -> DriverResult<Arc<SerialPort>> {
    let mmio = device.first_mmio().ok_or(DriverError::InvalidInput)?;
    let paddr = PhysAddr::from_usize(mmio.base);
    let ident = SerialIdent::Mmio {
        paddr,
        size: mmio.size,
    };

    // The early stdout instance shares printk's hardware, so adopt it verbatim;
    // it already carries `SerialRole::Stdout` from early-boot construction.
    if let Some(early) = take_early_port(&ident) {
        return Ok(early);
    }

    let ptr = crate::devm_iomap(device, mmio, "serial")?;
    let vaddr = VirtAddr::from_usize(ptr.as_ptr() as usize);
    let port = match kind {
        #[cfg(feature = "serial-pl011")]
        SerialKind::Pl011 => {
            SerialPort::new_mmio_pl011(vaddr, paddr, mmio.size, SerialRole::Auxiliary)
        }
        #[cfg(feature = "serial-ns16550-mmio")]
        SerialKind::Ns16550Mmio => {
            let layout = console_driver::ns16550_mmio_layout_for_paddr(mmio.base)
                .ok_or(DriverError::InvalidInput)?;
            // SAFETY: `vaddr` is the device's exclusively-mapped NS16550 MMIO
            // window returned by `devm_iomap`. `layout` was decoded from the
            // matching device-tree node and supplies the register stride and
            // access width.
            unsafe {
                SerialPort::new_mmio_ns16550(
                    vaddr,
                    paddr,
                    mmio.size,
                    SerialRole::Auxiliary,
                    layout.reg_shift,
                    layout.reg_width,
                )
            }
        }
    };
    Ok(Arc::new(port))
}

/// Publish a resolved port: the stdout UART is registered as the active console
/// (so the TTY handoff adopts it); any other UART becomes a standalone char
/// device. `register_console_runtime` adds only that one device to the console
/// subsystem, so an auxiliary UART can never displace the real stdout UART.
fn publish(device: Arc<DeviceObject>, port: Arc<SerialPort>) -> DriverResult<()> {
    let role = port.role();
    let dev = Box::new(SerialCharDev(port));
    match role {
        SerialRole::Stdout => runtime::register_console_runtime(device, dev),
        SerialRole::Auxiliary => publish_char(device, dev),
    }
}

// --- PL011 -------------------------------------------------------------------

#[cfg(feature = "serial-pl011")]
const PL011: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "serial-pl011",
    dt_compatibles: &["arm,pl011"],
    acpi_ids: &[],
};

#[cfg(feature = "serial-pl011")]
struct Pl011SerialDriver;

#[cfg(feature = "serial-pl011")]
impl DeviceDriver for Pl011SerialDriver {
    fn name(&self) -> &'static str {
        "serial-pl011"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &PL011
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        let port = resolve_port(device.as_ref(), SerialKind::Pl011)?;
        publish(device, port)
    }
}

#[cfg(feature = "serial-pl011")]
pub(super) fn pl011_descriptor() -> BoxedDriver {
    Arc::new(Pl011SerialDriver)
}

// --- NS16550 MMIO ------------------------------------------------------------

#[cfg(feature = "serial-ns16550-mmio")]
const NS16550_MMIO: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "serial-ns16550-mmio",
    dt_compatibles: &["ns16550a", "ns16550", "snps,dw-apb-uart", "mrvl,mmp-uart"],
    acpi_ids: &[],
};

#[cfg(feature = "serial-ns16550-mmio")]
struct Ns16550MmioSerialDriver;

#[cfg(feature = "serial-ns16550-mmio")]
impl DeviceDriver for Ns16550MmioSerialDriver {
    fn name(&self) -> &'static str {
        "serial-ns16550-mmio"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &NS16550_MMIO
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        let port = resolve_port(device.as_ref(), SerialKind::Ns16550Mmio)?;
        publish(device, port)
    }
}

#[cfg(feature = "serial-ns16550-mmio")]
pub(super) fn ns16550_mmio_descriptor() -> BoxedDriver {
    Arc::new(Ns16550MmioSerialDriver)
}

// --- NS16550 I/O-port (x86 ISA COM) ------------------------------------------

/// Matches the platform-static ioport device registered by the platform backend
/// (x86 has no device tree, so this is static-only: empty DT compatibles).
#[cfg(feature = "serial-ns16550-ioport")]
const NS16550_IOPORT: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "serial-ns16550-ioport",
    dt_compatibles: &[],
    acpi_ids: &[],
};

#[cfg(feature = "serial-ns16550-ioport")]
struct Ns16550IoPortSerialDriver;

#[cfg(feature = "serial-ns16550-ioport")]
impl DeviceDriver for Ns16550IoPortSerialDriver {
    fn name(&self) -> &'static str {
        "serial-ns16550-ioport"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Char
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BusTypeId::PLATFORM]
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &NS16550_IOPORT
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        // The x86 ISA COM port is a single fixed port with no per-device
        // resource and no device-tree node, so always adopt the early stdout
        // instance (no remap, no fresh port).
        let ident = SerialIdent::IoPort {
            port: console_driver::boot_console_io_port(),
        };
        let port = take_early_port(&ident).ok_or(DriverError::BadState)?;
        publish(device, port)
    }
}

#[cfg(feature = "serial-ns16550-ioport")]
pub(super) fn ns16550_ioport_descriptor() -> BoxedDriver {
    Arc::new(Ns16550IoPortSerialDriver)
}
