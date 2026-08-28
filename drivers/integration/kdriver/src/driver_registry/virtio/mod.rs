// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO driver implementations for the descriptor-first driver core.
//!
//! Each VirtIO device type (net, blk, gpu, input, vsock, 9p) gets a
//! [`DeviceDriver`] impls that match the optional VirtIO type carried by the
//! PCI or platform identity produced during discovery.

pub(crate) mod ids;

#[cfg(feature = "virtio")]
mod glue;

#[cfg(feature = "virtio")]
use alloc::sync::Arc;

#[cfg(feature = "virtio")]
use driver_base::{DeviceKind, DriverError, DriverResult};
#[cfg(feature = "virtio")]
use kdevice::{
    BusTypeId, DeviceDriver, DeviceLocation, DeviceMatcher, DeviceObject, VirtioTypeMatcher,
};

#[cfg(feature = "virtio")]
use self::ids::virtio_type;
#[cfg(feature = "virtio")]
use crate::driver_registry::BoxedDriver;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct VirtioDriver {
    name: &'static str,
    kind: DeviceKind,
    device_type: u32,
    bus_type: BusTypeId,
    matcher: VirtioTypeMatcher,
}

impl DeviceDriver for VirtioDriver {
    fn name(&self) -> &'static str {
        self.name
    }

    fn device_kind(&self) -> DeviceKind {
        self.kind
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        if self.bus_type == BusTypeId::PCI {
            &[BusTypeId::PCI]
        } else {
            &[BusTypeId::PLATFORM]
        }
    }

    fn matcher(&self) -> &dyn DeviceMatcher {
        &self.matcher
    }

    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        activate_virtio_device(device, self.kind, self.device_type)
    }
}

fn make_virtio_driver(
    name: &'static str,
    kind: DeviceKind,
    device_type: u32,
    bus_type: BusTypeId,
) -> BoxedDriver {
    Arc::new(VirtioDriver {
        name,
        kind,
        device_type,
        bus_type,
        matcher: VirtioTypeMatcher { device_type },
    })
}

macro_rules! virtio_driver_pair {
    (
        $pci_ctor:ident, $mmio_ctor:ident, $kind:expr, $vtype:expr, $pci_name:expr, $mmio_name:expr
    ) => {
        pub fn $pci_ctor() -> BoxedDriver {
            make_virtio_driver($pci_name, $kind, $vtype, BusTypeId::PCI)
        }

        pub fn $mmio_ctor() -> BoxedDriver {
            make_virtio_driver($mmio_name, $kind, $vtype, BusTypeId::PLATFORM)
        }
    };
}

// ---------------------------------------------------------------------------
// Shared activate logic (PCI path — type already known)
// ---------------------------------------------------------------------------

/// Activate a VirtIO device whose type is known (PCI path).
#[cfg(feature = "virtio")]
#[allow(unused_variables)]
fn activate_virtio_device(
    device: Arc<DeviceObject>,
    device_kind: DeviceKind,
    vtype: u32,
) -> DriverResult<()> {
    match device.location() {
        DeviceLocation::Pci { .. } => activate_virtio_pci(device, device_kind),
        DeviceLocation::Mmio { .. }
            if matches!(
                device.transport(),
                Some(kdevice::TransportInfo::Virtio { device_type }) if device_type == vtype
            ) =>
        {
            activate_virtio_mmio(device, device_kind)
        }
        _ => {
            log::warn!("virtio probe: unsupported location {:?}", device.location());
            Err(DriverError::Unsupported)
        }
    }
}

// ---------------------------------------------------------------------------
// PCI transport activate + dispatch
// ---------------------------------------------------------------------------

#[cfg(feature = "virtio")]
fn activate_virtio_pci(device: Arc<DeviceObject>, device_kind: DeviceKind) -> DriverResult<()> {
    use pci::{DeviceFunction, PciBus};

    let (segment, bus_nr, dev_nr, function) = match device.location() {
        DeviceLocation::Pci {
            segment,
            bus,
            device,
            function,
        } => (segment, bus, device, function),
        _ => return Err(DriverError::InvalidInput),
    };

    let _ = segment; // segment 0 only for now

    // Re-open the bus to get a `PciRoot` for `probe_pci_device`. The ECAM
    // mapping is idempotent (`memspace::iomap_device` returns the cached
    // mapping) and `configure_pci_device_if_needed` is a no-op because BAR
    // configuration was already performed during PCI bus discovery.
    let mut pci_bus = PciBus::new(crate::bus::pci_support::pci_cam_kind()).map_err(|err| {
        log::warn!("virtio PCI activate: PciBus::new failed: {:?}", err);
        DriverError::Io
    })?;
    let bdf = DeviceFunction {
        bus: bus_nr,
        device: dev_nr,
        function,
    };

    let (root, config) = pci_bus.parts_mut();
    crate::bus::pci_support::configure_pci_device_if_needed(root, bdf)?;
    let dev_info = root
        .enumerate_bus(bus_nr)
        .find(|(b, _)| *b == bdf)
        .map(|(_, info)| info)
        .ok_or(DriverError::BadState)?;

    let (ty, transport, irq) =
        virtio::probe_pci_device::<glue::VirtIoHalImpl, pci::MmioCam<'static>>(
            root,
            bdf,
            &dev_info,
            config,
            crate::resource::resource_provider(),
        )
        .ok_or(DriverError::BadState)?;

    if ty != device_kind {
        return Err(DriverError::Unsupported);
    }

    dispatch_virtio_try_new(device, device_kind, transport, Some(irq))
}

#[cfg(feature = "virtio")]
fn activate_virtio_mmio(device: Arc<DeviceObject>, device_kind: DeviceKind) -> DriverResult<()> {
    let (base, size) = match device.location() {
        DeviceLocation::Mmio { base, size } => (base, size),
        _ => return Err(DriverError::InvalidInput),
    };

    let regs = crate::iomap_mmio(base, size, "virtio-mmio-transport")?;
    let irq = device.first_irq().map(|resource| resource.number);
    // SAFETY: `regs` was obtained from `iomap_mmio` which maps a valid
    // physical MMIO region, and `size` matches the region size.
    let (ty, transport) =
        unsafe { virtio::probe_mmio_device(regs.as_ptr(), size) }.ok_or(DriverError::BadState)?;

    if ty != device_kind {
        return Err(DriverError::Unsupported);
    }

    dispatch_virtio_try_new(device, device_kind, transport, irq)
}

#[cfg(feature = "virtio")]
fn dispatch_virtio_try_new<T: virtio::Transport + 'static>(
    parent: Arc<DeviceObject>,
    kind: DeviceKind,
    transport: T,
    irq: Option<usize>,
) -> DriverResult<()> {
    match kind {
        #[cfg(feature = "virtio-net")]
        DeviceKind::Net => {
            use glue::VirtIoNet;
            kclass::publish_net(parent, VirtIoNet::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-blk")]
        DeviceKind::Block => {
            use glue::VirtIoBlk;
            kclass::publish_block(parent, VirtIoBlk::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-gpu")]
        DeviceKind::Display => {
            use glue::VirtIoGpu;
            kclass::publish_display(parent, VirtIoGpu::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-input")]
        DeviceKind::Input => {
            use glue::VirtIoInput;
            kclass::publish_input(parent, VirtIoInput::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-socket")]
        DeviceKind::Vsock => {
            use glue::VirtIoSocket;
            kclass::publish_vsock(parent, VirtIoSocket::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-9p")]
        DeviceKind::Fs9p => {
            use glue::VirtIo9p;
            kclass::publish_virtio_9p(parent, VirtIo9p::try_new(transport, irq)?).map(drop)
        }
        #[cfg(feature = "virtio-rng")]
        DeviceKind::Char => {
            use glue::VirtIoRng;
            kclass::publish_char(parent, VirtIoRng::try_new(transport, irq)?).map(drop)
        }
        _ => Err(DriverError::Unsupported),
    }
}

// ---------------------------------------------------------------------------
// Concrete drivers for each VirtIO device type
// ---------------------------------------------------------------------------

#[cfg(feature = "virtio-net")]
virtio_driver_pair!(
    virtio_net_pci_descriptor,
    virtio_net_mmio_descriptor,
    DeviceKind::Net,
    virtio_type::NET,
    "virtio-net-pci",
    "virtio-net-mmio"
);

#[cfg(feature = "virtio-blk")]
virtio_driver_pair!(
    virtio_blk_pci_descriptor,
    virtio_blk_mmio_descriptor,
    DeviceKind::Block,
    virtio_type::BLOCK,
    "virtio-blk-pci",
    "virtio-blk-mmio"
);

#[cfg(feature = "virtio-gpu")]
virtio_driver_pair!(
    virtio_gpu_pci_descriptor,
    virtio_gpu_mmio_descriptor,
    DeviceKind::Display,
    virtio_type::GPU,
    "virtio-gpu-pci",
    "virtio-gpu-mmio"
);

#[cfg(feature = "virtio-input")]
virtio_driver_pair!(
    virtio_input_pci_descriptor,
    virtio_input_mmio_descriptor,
    DeviceKind::Input,
    virtio_type::INPUT,
    "virtio-input-pci",
    "virtio-input-mmio"
);

#[cfg(feature = "virtio-socket")]
virtio_driver_pair!(
    virtio_vsock_pci_descriptor,
    virtio_vsock_mmio_descriptor,
    DeviceKind::Vsock,
    virtio_type::VSOCK,
    "virtio-vsock-pci",
    "virtio-vsock-mmio"
);

#[cfg(feature = "virtio-9p")]
virtio_driver_pair!(
    virtio_9p_pci_descriptor,
    virtio_9p_mmio_descriptor,
    DeviceKind::Fs9p,
    virtio_type::NINEP,
    "virtio-9p-pci",
    "virtio-9p-mmio"
);

#[cfg(feature = "virtio-rng")]
virtio_driver_pair!(
    virtio_rng_pci_descriptor,
    virtio_rng_mmio_descriptor,
    DeviceKind::Char,
    virtio_type::RNG,
    "virtio-rng-pci",
    "virtio-rng-mmio"
);

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "virtio-net")]
    virtio_net_pci_descriptor,
    #[cfg(feature = "virtio-net")]
    virtio_net_mmio_descriptor,
    #[cfg(feature = "virtio-blk")]
    virtio_blk_pci_descriptor,
    #[cfg(feature = "virtio-blk")]
    virtio_blk_mmio_descriptor,
    #[cfg(feature = "virtio-gpu")]
    virtio_gpu_pci_descriptor,
    #[cfg(feature = "virtio-gpu")]
    virtio_gpu_mmio_descriptor,
    #[cfg(feature = "virtio-input")]
    virtio_input_pci_descriptor,
    #[cfg(feature = "virtio-input")]
    virtio_input_mmio_descriptor,
    #[cfg(feature = "virtio-socket")]
    virtio_vsock_pci_descriptor,
    #[cfg(feature = "virtio-socket")]
    virtio_vsock_mmio_descriptor,
    #[cfg(feature = "virtio-9p")]
    virtio_9p_pci_descriptor,
    #[cfg(feature = "virtio-9p")]
    virtio_9p_mmio_descriptor,
    #[cfg(feature = "virtio-rng")]
    virtio_rng_pci_descriptor,
    #[cfg(feature = "virtio-rng")]
    virtio_rng_mmio_descriptor,
];

/// Register all enabled VirtIO drivers with the given registrar.
#[cfg(feature = "virtio")]
pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
