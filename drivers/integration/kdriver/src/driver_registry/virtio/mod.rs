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

#[cfg(feature = "virtio-gpu")]
mod gpu;

#[cfg(feature = "virtio-blk")]
mod block;

#[cfg(feature = "virtio-input")]
mod input;

#[cfg(feature = "virtio")]
use driver_base::{DeviceKind, DriverError, DriverResult};
#[cfg(feature = "virtio")]
use driver_os_api::{IrqResource, MmioClaim};
#[cfg(feature = "virtio")]
use kdevice::{
    BusTypeId, DeviceDesc, DeviceDriver, DeviceLocation, DeviceMatcher, DeviceObject,
    FirmwareMatchSpec, MatchResult, VirtioTypeMatcher,
};

#[cfg(feature = "virtio")]
use self::ids::{VIRTIO_MMIO_COMPATIBLE, virtio_type};
#[cfg(feature = "virtio")]
use crate::driver_registry::BoxedDriver;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Matcher chosen by the transport the driver is registered for.
///
/// PCI descriptors carry the VirtIO type discovered from config space, so the
/// PCI driver keeps [`VirtioTypeMatcher`]. Platform descriptors only carry the
/// `virtio,mmio` firmware identity; the concrete type is decided during probe.
enum VirtioMatcher {
    Pci(VirtioTypeMatcher),
    Mmio(FirmwareMatchSpec),
}

impl DeviceMatcher for VirtioMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        match self {
            Self::Pci(matcher) => matcher.matches(desc),
            Self::Mmio(matcher) => matcher.matches(desc),
        }
    }

    fn firmware_spec(&self) -> Option<&FirmwareMatchSpec> {
        match self {
            Self::Pci(_) => None,
            Self::Mmio(spec) => Some(spec),
        }
    }
}

struct VirtioDriver {
    name: &'static str,
    kind: DeviceKind,
    device_type: u32,
    bus_type: BusTypeId,
    matcher: VirtioMatcher,
}

enum VirtioBindingData {
    NoPrivateState,
    #[cfg(feature = "virtio-blk")]
    Block(block::BlockActivation),
}

impl DeviceDriver for VirtioDriver {
    type Data = VirtioBindingData;

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

    fn probe_device(&self, ctx: &kdevice::ProbeContext<'_>) -> DriverResult<Self::Data> {
        activate_virtio_device(ctx, self.bus_type, self.kind, self.device_type)
    }

    fn remove(
        &self,
        _ctx: &kdevice::RemoveContext<'_>,
        data: Option<&mut Self::Data>,
    ) -> DriverResult<()> {
        #[cfg(feature = "virtio-blk")]
        if let Some(VirtioBindingData::Block(activation)) = data {
            activation.close();
        }
        #[cfg(not(feature = "virtio-blk"))]
        let _ = data;
        Ok(())
    }
}

fn make_virtio_driver(
    name: &'static str,
    kind: DeviceKind,
    device_type: u32,
    bus_type: BusTypeId,
) -> BoxedDriver {
    let matcher = if bus_type == BusTypeId::PCI {
        VirtioMatcher::Pci(VirtioTypeMatcher { device_type })
    } else {
        VirtioMatcher::Mmio(FirmwareMatchSpec {
            // The alias is only consulted for platform-static devices; using
            // the driver name keeps this matcher from matching one by accident.
            alias: name,
            dt_compatibles: &[VIRTIO_MMIO_COMPATIBLE],
            acpi_ids: &[],
        })
    };
    BoxedDriver::new(VirtioDriver {
        name,
        kind,
        device_type,
        bus_type,
        matcher,
    })
}

macro_rules! virtio_driver_pair {
    (
        $pci_ctor:ident, $mmio_ctor:ident, $kind:expr, $vtype:expr, $pci_name:expr, $mmio_name:expr
    ) => {
        pub(crate) fn $pci_ctor() -> BoxedDriver {
            make_virtio_driver($pci_name, $kind, $vtype, BusTypeId::PCI)
        }

        pub(crate) fn $mmio_ctor() -> BoxedDriver {
            make_virtio_driver($mmio_name, $kind, $vtype, BusTypeId::PLATFORM)
        }
    };
}

// ---------------------------------------------------------------------------
// Shared activate logic
// ---------------------------------------------------------------------------

/// Activate a VirtIO device using the path selected by its bus.
///
/// The PCI path already knows the VirtIO type from config space; the platform
/// path must identify the device from its read-only MMIO header before it can
/// decide.
#[cfg(feature = "virtio")]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Device activation inputs are used only by enabled VirtIO \
              device features"
)]
#[allow(
    unused_variables,
    reason = "CG-024-lint-exceptions: unused_variables: Device activation inputs are used only by \
              enabled VirtIO device features"
)]
fn activate_virtio_device(
    ctx: &kdevice::ProbeContext<'_>,
    bus_type: BusTypeId,
    device_kind: DeviceKind,
    vtype: u32,
) -> DriverResult<VirtioBindingData> {
    if bus_type == BusTypeId::PCI {
        activate_virtio_pci(ctx, device_kind)
    } else if bus_type == BusTypeId::PLATFORM {
        activate_virtio_mmio(ctx, device_kind, vtype)
    } else {
        log::warn!(
            "virtio {device_kind:?} at {:?}: unsupported bus type {}",
            ctx.device().location(),
            bus_type.name()
        );
        Err(DriverError::Unsupported)
    }
}

// ---------------------------------------------------------------------------
// PCI transport activate + dispatch
// ---------------------------------------------------------------------------

#[cfg(feature = "virtio")]
fn activate_virtio_pci(
    ctx: &kdevice::ProbeContext<'_>,
    device_kind: DeviceKind,
) -> DriverResult<VirtioBindingData> {
    use pci::{DeviceFunction, PciBus};

    let device = ctx.device();
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

    let intx = device.first_irq();

    let probed = virtio::probe_pci_device::<glue::VirtIoHalImpl, pci::MmioCam<'static>>(
        root,
        bdf,
        &dev_info,
        config,
        intx,
        crate::resource::resource_provider(),
    );

    // Only an explicit "no interrupt" probe result is related to this
    // device's INTx discovery status. A transport failure stays a transport
    // failure even when the device also has no routed interrupt.
    let (ty, transport, irq) = match probed {
        Ok(probed) => probed,
        Err(virtio::PciProbeError::NotVirtio) => return Err(DriverError::Unsupported),
        Err(virtio::PciProbeError::Transport) => return Err(DriverError::Io),
        Err(virtio::PciProbeError::NoInterrupt) => {
            return Err(no_interrupt_error(device.as_ref(), device_kind));
        }
    };

    if ty != device_kind {
        return Err(DriverError::Unsupported);
    }

    #[cfg(feature = "virtio-gpu")]
    if device_kind == DeviceKind::Display {
        return gpu::activate(ctx, transport, Some(irq), Some((config, bdf)))
            .map(|_| VirtioBindingData::NoPrivateState);
    }
    #[cfg(feature = "virtio-input")]
    if device_kind == DeviceKind::Input {
        return input::activate_pci(ctx, transport, irq, config, bdf)
            .map(|_| VirtioBindingData::NoPrivateState);
    }

    dispatch_virtio_try_new(ctx, device_kind, transport, Some(irq))
}

/// Arm this function after its handler is live. LIFO cleanup masks just the
/// function before its shared IRQ action is removed.
#[cfg(any(feature = "virtio-gpu", feature = "virtio-input"))]
fn arm_pci_intx(
    ctx: &kdevice::ProbeContext<'_>,
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
) -> DriverResult<()> {
    let mut cleanup_config = *config;
    ctx.add_cleanup(move || set_pci_intx_disabled(&mut cleanup_config, bdf, true))?;
    set_pci_intx_disabled(config, bdf, false);
    Ok(())
}

/// Converts an explicit `NoInterrupt` probe result into an activation error.
///
/// The discovery status stored on the device is driver-neutral; this policy
/// layer is the only place that relates it to the activation failure. The
/// detailed firmware error was logged by the PCI INTx resolver.
#[cfg(feature = "virtio")]
fn no_interrupt_error(device: &DeviceObject, device_kind: DeviceKind) -> DriverError {
    #[cfg(target_arch = "x86_64")]
    {
        match device.intx_route() {
            Some(kdevice::IntxRouteStatus::NotConnected) => {
                log::warn!(
                    "virtio {device_kind:?} at {:?}: no interrupt and firmware reports no INTx \
                     connection",
                    device.location()
                );
                DriverError::Unsupported
            }
            Some(kdevice::IntxRouteStatus::Failed) => {
                log::error!(
                    "virtio {device_kind:?} at {:?}: no interrupt and INTx route resolution failed",
                    device.location()
                );
                DriverError::Io
            }
            Some(kdevice::IntxRouteStatus::Routed) => {
                log::error!(
                    "virtio {device_kind:?} at {:?}: no interrupt despite a routed INTx resource",
                    device.location()
                );
                DriverError::Io
            }
            None => {
                log::warn!(
                    "virtio {device_kind:?} at {:?}: no interrupt and no INTx discovery result",
                    device.location()
                );
                DriverError::Io
            }
        }
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        log::warn!(
            "virtio {device_kind:?} at {:?}: no interrupt available",
            device.location()
        );
        DriverError::BadState
    }
}

/// Decision made from a read-only MMIO header read.
#[cfg(feature = "virtio")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MmioMatch {
    /// The header reports the type this driver handles.
    Match,
    /// The header reports no device (VirtIO device ID 0).
    EmptySlot,
    /// The header reports a different, supported device.
    Mismatch { found: u32 },
}

/// Classifies a raw MMIO device ID against the type a driver declares.
#[cfg(feature = "virtio")]
fn classify_mmio_device(device_id: u32, expected: u32) -> MmioMatch {
    match device_id {
        0 => MmioMatch::EmptySlot,
        found if found == expected => MmioMatch::Match,
        found => MmioMatch::Mismatch { found },
    }
}

/// Converts a read-only identification error into a driver error.
#[cfg(feature = "virtio")]
fn mmio_error(error: virtio::MmioError) -> DriverError {
    match error {
        virtio::MmioError::MmioRegionTooSmall | virtio::MmioError::Misaligned => {
            DriverError::InvalidInput
        }
        _ => DriverError::Io,
    }
}

/// One reference to an MMIO mapping obtained from [`crate::iomap_mmio`],
/// together with the exclusive claim on the mapped physical range.
///
/// [`crate::iomap_mmio`] takes a reference on every call, including cached
/// hits, so the probe owns exactly this reference. The successful transport
/// owns this guard, which makes the release happen after the transport's last
/// register access and its `Drop` reset; failed probes drop it on return.
/// The claim is released after `iounmap` because fields drop in declaration
/// order after this type's `Drop` body.
#[cfg(feature = "virtio")]
struct MmioMapping {
    vaddr: memaddr::VirtAddr,
    _claim: MmioClaim,
}

#[cfg(feature = "virtio")]
impl MmioMapping {
    fn new(vaddr: memaddr::VirtAddr, claim: MmioClaim) -> Self {
        Self {
            vaddr,
            _claim: claim,
        }
    }
}

#[cfg(feature = "virtio")]
impl Drop for MmioMapping {
    fn drop(&mut self) {
        let _ = memspace::iounmap(self.vaddr);
    }
}

/// Probe-time access to a VirtIO MMIO region.
///
/// The production implementation maps a physical region and holds the shared
/// [`MmioClaim`] on its range; unit tests substitute a fixture so the real
/// candidate chain, claim ordering and decision order can be exercised without
/// hardware.
#[cfg(feature = "virtio")]
trait MmioAccess {
    /// Transport opened by [`MmioAccess::open`].
    type Transport: virtio::Transport + Send + 'static;

    /// Identifies the device from read-only header fields.
    fn identify(&mut self, size: usize) -> Result<virtio::MmioIdentification, DriverError>;

    /// Opens the transport. Must only be called after identification matched.
    fn open(self, size: usize) -> DriverResult<Self::Transport>;

    /// Whether this access source is the synthetic unit-test fixture.
    ///
    /// Only used to select a test-only counting HAL for the same production
    /// driver construction. The production implementation keeps this false.
    #[cfg(unittest)]
    fn is_fixture(&self) -> bool {
        false
    }
}

/// A mapped MMIO region plus the reference that keeps it mapped.
#[cfg(feature = "virtio")]
struct MappedMmio {
    regs: core::ptr::NonNull<u8>,
    mapping: MmioMapping,
}

#[cfg(feature = "virtio")]
impl MmioAccess for MappedMmio {
    type Transport = virtio::OwnedMmioTransport<MmioMapping>;

    fn identify(&mut self, size: usize) -> Result<virtio::MmioIdentification, DriverError> {
        // SAFETY: `regs` is the valid, aligned mapping described by
        // `mmio.size`. `mapping` holds the shared exclusive claim on this
        // physical range from before this call. Every participant of the
        // `driver_os_api::MmioClaimOp` contract is refused overlapping claims,
        // including a duplicate descriptor that `DeviceManager::rescan` may
        // register while a live transport still owns the range. Discovery no
        // longer maps or reads VirtIO MMIO registers, and no transport or
        // interrupt handler exists for this descriptor before this call, so no
        // other participant can alias the region while the read-only
        // `UniqueMmioPointer` exists.
        unsafe { virtio::identify_mmio_device(self.regs, size) }.map_err(|error| {
            log::warn!("read-only MMIO identification failed: {error:?}");
            mmio_error(error)
        })
    }

    fn open(self, size: usize) -> DriverResult<Self::Transport> {
        // SAFETY: Same mapping and exclusive-claim argument as `identify`; the
        // header matched this driver, so this is the device it describes. The
        // returned transport owns `mapping`, so both the mapping and its claim
        // outlive the transport's last access and `Drop`.
        let transport =
            unsafe { virtio::open_mmio_transport(self.regs, size) }.map_err(|error| {
                log::warn!("failed to open MMIO transport: {error:?}");
                mmio_error(error)
            })?;
        Ok(virtio::OwnedMmioTransport::new(transport, self.mapping))
    }
}

/// Runs the shared MMIO decision sequence: identify read-only, reject a
/// non-matching ID, and only then open the transport and dispatch it.
///
/// `access` only abstracts the mapping and transport source. In particular,
/// `open` is not called unless the raw device ID matched, so a wrong candidate
/// cannot construct a transport whose `Drop` resets the device.
#[cfg(feature = "virtio")]
fn activate_mmio_with<A: MmioAccess>(
    ctx: &kdevice::ProbeContext<'_>,
    device_kind: DeviceKind,
    vtype: u32,
    size: usize,
    mut access: A,
) -> DriverResult<VirtioBindingData> {
    let device = ctx.device();
    let identification = access.identify(size)?;

    match classify_mmio_device(identification.device_id, vtype) {
        MmioMatch::Match => {}
        MmioMatch::EmptySlot => {
            log::debug!(
                "virtio {device_kind:?} at {:?}: empty MMIO slot",
                device.location()
            );
            return Err(DriverError::Unsupported);
        }
        MmioMatch::Mismatch { found } => {
            log::debug!(
                "virtio {device_kind:?} at {:?}: device ID {found} is not virtio type {vtype}",
                device.location()
            );
            return Err(DriverError::Unsupported);
        }
    }

    #[cfg(unittest)]
    let fixture = access.is_fixture();
    let irq = device.first_irq();
    let transport = access.open(size)?;
    #[cfg(unittest)]
    if fixture {
        return dispatch_fixture_try_new(ctx, device_kind, transport, irq);
    }
    dispatch_virtio_try_new(ctx, device_kind, transport, irq)
}

/// Dispatches a fixture-opened transport.
///
/// VSOCK uses the counting wrapper over the production HAL so the test can
/// attribute DMA calls to this scenario; every other device keeps the
/// production dispatch unchanged.
#[cfg(all(feature = "virtio", unittest))]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the fixture dispatch transfers the production \
              boxed VsockDeviceImpl from the counting-HAL constructor to kclass publication"
)]
fn dispatch_fixture_try_new<T: virtio::Transport + Send + 'static>(
    ctx: &kdevice::ProbeContext<'_>,
    kind: DeviceKind,
    transport: T,
    irq: Option<IrqResource>,
) -> DriverResult<VirtioBindingData> {
    match kind {
        #[cfg(feature = "virtio-socket")]
        DeviceKind::Vsock => {
            use glue::VirtIoSocket;
            kclass::publish_vsock(ctx, VirtIoSocket::try_new_counting(transport, irq)?)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        _ => dispatch_virtio_try_new(ctx, kind, transport, irq),
    }
}

/// Activate a platform VirtIO device after identifying it read-only.
///
/// Every enabled MMIO driver matches every `virtio,mmio` descriptor, so this
/// runs for each candidate in registration order. Until the device ID matches,
/// only the magic/version/ID fields are read; the transport (whose `Drop`
/// resets the device) is constructed only for the driver that owns the device.
#[cfg(feature = "virtio")]
fn activate_virtio_mmio(
    ctx: &kdevice::ProbeContext<'_>,
    device_kind: DeviceKind,
    vtype: u32,
) -> DriverResult<VirtioBindingData> {
    let device = ctx.device();
    let Some(mmio) = device.first_mmio() else {
        log::warn!(
            "virtio {device_kind:?} at {:?}: platform device has no MMIO resource",
            device.location()
        );
        return Err(DriverError::InvalidInput);
    };

    // Claim the physical range through the shared resource contract before any
    // register access. The descriptor binding lock cannot establish physical
    // exclusivity: a rescan may register a second descriptor with a fresh lock
    // for the same firmware node while a live transport still owns the range.
    let claim =
        crate::resource::claim_mmio_region(mmio, "virtio-mmio-probe").inspect_err(|error| {
            log::warn!(
                "virtio {device_kind:?} at {:?}: cannot claim MMIO region {:#x}+{:#x}: {error:?}",
                device.location(),
                mmio.base,
                mmio.size,
            );
        })?;

    // The unit-test fixture replaces only the register source. It receives the
    // production claim, so the claim-before-access order stays under test.
    #[cfg(unittest)]
    if let Some(access) = virtio_mmio_support::fixture_for(mmio.base) {
        return activate_mmio_with(ctx, device_kind, vtype, mmio.size, access.with_claim(claim));
    }

    let regs = crate::iomap_mmio(mmio.base, mmio.size, "virtio-mmio-probe")?;
    let access = MappedMmio {
        regs,
        mapping: MmioMapping::new(memaddr::VirtAddr::from_mut_ptr(regs.as_ptr()), claim),
    };
    activate_mmio_with(ctx, device_kind, vtype, mmio.size, access)
}

#[cfg(feature = "virtio")]
fn dispatch_virtio_try_new<T: virtio::Transport + Send + 'static>(
    ctx: &kdevice::ProbeContext<'_>,
    kind: DeviceKind,
    transport: T,
    irq: Option<IrqResource>,
) -> DriverResult<VirtioBindingData> {
    match kind {
        #[cfg(feature = "virtio-net")]
        DeviceKind::Net => {
            use glue::VirtIoNet;
            kclass::publish_net(ctx, VirtIoNet::try_new(transport, irq)?)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        #[cfg(feature = "virtio-blk")]
        DeviceKind::Block => block::activate(ctx, transport, irq).map(VirtioBindingData::Block),
        #[cfg(feature = "virtio-gpu")]
        DeviceKind::Display => {
            gpu::activate(ctx, transport, irq, None)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        #[cfg(feature = "virtio-input")]
        DeviceKind::Input => {
            input::activate_mmio(ctx, transport, irq)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        #[cfg(feature = "virtio-socket")]
        DeviceKind::Vsock => {
            use glue::VirtIoSocket;
            kclass::publish_vsock(ctx, VirtIoSocket::try_new(transport, irq)?)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        #[cfg(feature = "virtio-9p")]
        DeviceKind::Fs9p => {
            use glue::VirtIo9p;
            kclass::publish_virtio_9p(ctx, VirtIo9p::try_new(transport, irq)?)?;
            Ok(VirtioBindingData::NoPrivateState)
        }
        #[cfg(feature = "virtio-rng")]
        DeviceKind::Hwrng => {
            use glue::VirtIoRng;

            let device = match (ctx.device().location(), ctx.device().dma()) {
                (DeviceLocation::Mmio { .. }, Some(info)) => {
                    VirtIoRng::try_new_with_dma(transport, irq, info)?
                }
                (DeviceLocation::Mmio { .. }, None) => {
                    log::warn!("virtio-rng: platform device has no resolved firmware DMA view");
                    return Err(DriverError::Unsupported);
                }
                (DeviceLocation::Pci { .. }, _) => VirtIoRng::try_new(transport, irq)?,
                _ => return Err(DriverError::Unsupported),
            };
            kclass::publish_hwrng(ctx, device)?;
            Ok(VirtioBindingData::NoPrivateState)
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
    DeviceKind::Hwrng,
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
pub(crate) fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}

/// Masks or unmasks legacy INTx delivery for one PCI function.
///
/// A transport that has reached `DRIVER_OK` may assert its level-triggered
/// line at any time. Activation that installs the interrupt owner later masks
/// INTx first and unmasks it only once the action is registered, so a shared
/// line never sees an unrouted assertion.
///
/// Every caller belongs to a device kind that installs its interrupt owner
/// late, so a configuration that enables `virtio` but none of those kinds —
/// virtcca enables virtio without display or input — has no user of this.
///
/// Only the device kinds that mask INTx at probe and unmask it after
/// registering their handler call this. A configuration with `virtio` but
/// neither kind (virtcca enables virtio without display or input) has no
/// caller, so the definition is expected to be dead there. `unittest` alone is
/// not a caller either: the input test module that names it is gated on
/// `virtio-input` as well.
#[cfg(feature = "virtio")]
#[cfg_attr(
    not(any(feature = "virtio-gpu", feature = "virtio-input")),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: probe-time INTx masking is shared by every \
                  device kind that installs its interrupt owner late; a virtio configuration may \
                  enable neither virtio-gpu nor virtio-input (virtcca does), leaving the helper \
                  unused. It is used as soon as either kind is enabled, which is every \
                  configuration that has a display or input device"
    )
)]
pub(super) fn set_pci_intx_disabled(
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
    is_disabled: bool,
) {
    const INTERRUPT_DISABLE: u16 = 1 << 10;
    const STATUS_COMMAND_OFFSET: u16 = 0x04;

    let status_command = config.read_word(bdf, STATUS_COMMAND_OFFSET);
    let command = if is_disabled {
        status_command as u16 | INTERRUPT_DISABLE
    } else {
        status_command as u16 & !INTERRUPT_DISABLE
    };
    // Status has write-one-to-clear bits in the same 32-bit word. Only write
    // the Command halfword so a pending Status bit is not cleared here.
    config.write_halfword(bdf, STATUS_COMMAND_OFFSET, command);
}

#[cfg(all(feature = "virtio", unittest))]
unittest::path_mod_test!(
    virtio_mmio_support,
    "../../../ut-tests/virtio_mmio_support.rs"
);

#[cfg(all(feature = "virtio", feature = "virtio-socket", unittest))]
unittest::path_mod_test!(
    virtio_mmio_claim_unittest,
    "../../../ut-tests/virtio_mmio_claim.rs"
);

unittest::path_mod_test!(
    virtio_mmio_match_unittest,
    "../../../ut-tests/virtio_mmio_match.rs"
);
