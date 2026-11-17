// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO PCI transport integration.

use driver_base::DeviceKind;
#[cfg(any(not(target_arch = "x86_64"), unittest))]
use driver_os_api::{IrqController, IrqRouteDesc, IrqTrigger};
use driver_os_api::{IrqOp, IrqResource};
use pci::PciConfigAccess;
#[cfg(not(target_arch = "x86_64"))]
use pci::{
    InterruptControllerKind, InterruptTrigger, LegacyInterruptRoute, legacy_interrupt_route,
};
#[cfg(target_arch = "x86_64")]
mod x86_64;
use virtio_drivers::{
    PhysAddr, Result as VirtIoResult,
    transport::{
        DeviceStatus, DeviceType, InterruptStatus, Transport,
        pci::{
            PciTransport as RawPciTransport, VirtioPciError,
            bus::{Command, ConfigurationAccess, DeviceFunction, DeviceFunctionInfo, PciRoot},
            virtio_device_type,
        },
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

#[cfg(target_arch = "x86_64")]
use self::x86_64::{PciIrqState, PciMsixState, setup_msix};
use crate::{VirtIoHal, as_device_kind};

/// PCI transport wrapper with x86 MSI-X queue-vector setup.
pub struct PciTransport {
    inner: RawPciTransport,
    #[cfg(target_arch = "x86_64")]
    irq_state: PciIrqState,
}

#[cfg(target_arch = "x86_64")]
type PciMsixStateOption = Option<PciMsixState>;

#[cfg(not(target_arch = "x86_64"))]
type PciMsixStateOption = Option<()>;

impl PciTransport {
    /// Permanently close MSI-X delivery and release its vector allocation.
    ///
    /// Call after stopping device submissions, ordering already-issued messages,
    /// and draining/removing every IRQ action, in the provider's sleepable release
    /// context. This does not stop DMA or drain messages. Do not resume device operations
    /// after calling it, including after a failed attempt.
    ///
    /// # Errors
    /// On rejection the disabled transport retains its exact allocation for a
    /// later retry. Drop uses the infallible release contract instead: it must
    /// not silently discard a rejected allocation. Non-MSI transports are unchanged.
    #[cfg(target_arch = "x86_64")]
    pub fn try_release_msix(&mut self) -> driver_os_api::ResResult<()> {
        self.irq_state.try_release()
    }

    /// Construct a new PCI VirtIO transport.
    pub fn new<H: VirtIoHal, C: ConfigurationAccess>(
        root: &mut PciRoot<C>,
        bdf: DeviceFunction,
    ) -> Result<Self, VirtioPciError> {
        Self::new_with_msix::<H, C>(root, bdf, None)
    }

    fn new_with_msix<H: VirtIoHal, C: ConfigurationAccess>(
        root: &mut PciRoot<C>,
        bdf: DeviceFunction,
        #[cfg_attr(
            not(target_arch = "x86_64"),
            expect(
                unused_variables,
                reason = "CG-024-lint-exceptions: unused_variables: MSI-X transport setup is \
                          implemented only on x86_64"
            )
        )]
        msix: PciMsixStateOption,
    ) -> Result<Self, VirtioPciError> {
        let inner = match RawPciTransport::new::<H, C>(root, bdf) {
            Ok(inner) => inner,
            Err(err) => {
                #[cfg(target_arch = "x86_64")]
                if let Some(msix) = msix {
                    msix.release();
                }
                return Err(err);
            }
        };
        Ok(Self {
            inner,
            #[cfg(target_arch = "x86_64")]
            irq_state: PciIrqState::new(msix),
        })
    }

    #[cfg(target_arch = "x86_64")]
    fn fail_msix(&mut self) {
        self.irq_state.fail();
        let failed_status =
            (self.inner.get_status() | DeviceStatus::FAILED) & !DeviceStatus::DRIVER_OK;
        self.inner.set_status(failed_status);
    }
}

impl Transport for PciTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.inner.write_driver_features(driver_features);
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        #[cfg(target_arch = "x86_64")]
        let max_size = {
            if self.irq_state.is_failed() {
                return 0;
            }

            let max_size = self.inner.max_queue_size(queue);
            if max_size == 0 {
                return 0;
            }

            let msix_failed = if let Some(msix) = self.irq_state.msix_mut()
                && !msix.set_queue_vector(queue)
            {
                Some(msix.bdf())
            } else {
                None
            };
            if let Some(bdf) = msix_failed {
                log::warn!(
                    "PCI virtio device at {:?}: MSI-X queue {} vector setup failed",
                    bdf,
                    queue
                );
                self.fail_msix();
                return 0;
            }

            max_size
        };

        #[cfg(not(target_arch = "x86_64"))]
        let max_size = self.inner.max_queue_size(queue);

        max_size
    }

    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue);
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        #[cfg(target_arch = "x86_64")]
        if self.irq_state.is_failed() && status.contains(DeviceStatus::DRIVER_OK) {
            let failed_status = (status | DeviceStatus::FAILED) & !DeviceStatus::DRIVER_OK;
            self.inner.set_status(failed_status);
            return;
        }

        self.inner.set_status(status);
        #[cfg(target_arch = "x86_64")]
        if status.contains(DeviceStatus::DRIVER_OK)
            && let Some(msix) = self.irq_state.msix_mut()
        {
            msix.activate();
        }

        #[cfg(target_arch = "x86_64")]
        let config_msix_failed = if status.contains(DeviceStatus::FEATURES_OK)
            && !status.contains(DeviceStatus::DRIVER_OK)
            && let Some(msix) = self.irq_state.msix_mut()
            && !msix.set_config_vector()
        {
            Some(msix.bdf())
        } else {
            None
        };
        #[cfg(target_arch = "x86_64")]
        if let Some(bdf) = config_msix_failed {
            log::warn!(
                "PCI virtio device at {:?}: MSI-X config vector setup failed",
                bdf
            );
            self.fail_msix();
        }
    }

    fn reset(&mut self) {
        // Forward instead of relying on the trait default: this wrapper's
        // `set_status` also drives MSI-X state, while the inner transport owns
        // the write-zero-and-poll reset semantics.
        self.inner.reset();
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.inner.set_guest_page_size(guest_page_size);
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.inner
            .queue_set(queue, size, descriptors, driver_area, device_area);
    }

    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue);
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.inner.ack_interrupt()
    }

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> VirtIoResult<T> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> VirtIoResult<()> {
        self.inner.write_config_space(offset, value)
    }
}

/// Why probing a PCI VirtIO device produced no usable device.
///
/// The variants are deliberately driver-neutral: the caller decides how to
/// report them and may combine `NoInterrupt` with its own discovery facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PciProbeError {
    /// The function is not a recognized VirtIO device.
    NotVirtio,
    /// No usable interrupt could be established, by MSI-X or legacy INTx.
    NoInterrupt,
    /// The PCI transport could not be constructed.
    Transport,
}

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type, a transport object
/// for later operations, and the interrupt resource it will use.
pub fn probe_pci_device<H: VirtIoHal, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
    config: &mut PciConfigAccess,
    intx: Option<IrqResource>,
    irq_provider: &'static dyn IrqOp,
) -> Result<(DeviceKind, PciTransport, IrqResource), PciProbeError> {
    let dev_kind = virtio_device_type(dev_info)
        .and_then(as_device_kind)
        .ok_or(PciProbeError::NotVirtio)?;

    #[cfg(target_arch = "x86_64")]
    let (irq, msix) = {
        // MSI-X setup is limited to network devices for this x86 virtio-net
        // latency fix. Other virtio device kinds keep legacy IRQs until their
        // interrupt behavior is audited with the shared queue-vector setup.
        if dev_kind == DeviceKind::Net {
            match setup_msix::<H, C>(root, bdf, config, irq_provider) {
                Some((irq, msix)) => (irq, Some(msix)),
                None => (intx.ok_or(PciProbeError::NoInterrupt)?, None),
            }
        } else {
            (intx.ok_or(PciProbeError::NoInterrupt)?, None)
        }
    };

    #[cfg(not(target_arch = "x86_64"))]
    let irq = intx
        .or_else(|| legacy_irq_for_bdf(config, bdf, irq_provider))
        .ok_or(PciProbeError::NoInterrupt)?;

    #[cfg(not(target_arch = "x86_64"))]
    let msix = None;

    let transport = PciTransport::new_with_msix::<H, C>(root, bdf, msix).map_err(|error| {
        log::warn!("PCI virtio device at {bdf:?}: transport creation failed: {error:?}");
        PciProbeError::Transport
    })?;
    // Transport creation drives the device to `DRIVER_OK`, after which it may
    // raise INTx at any moment. Every kind whose activation installs the
    // interrupt owner later must mask INTx here, or a level-triggered line
    // fires with no action to claim it: on a shared line the storm starves the
    // CPU before the owner is ever registered. Display is included for the
    // same reason: activation installs its queue/config handler before unmasking
    // this function and enabling the device.
    if matches!(
        dev_kind,
        DeviceKind::Vsock | DeviceKind::Input | DeviceKind::Display
    ) {
        let (_, command) = root.get_status_command(bdf);
        root.set_command(bdf, command | Command::INTERRUPT_DISABLE);
    }
    log::info!("PCI virtio device at {:?}: IRQ = {}", bdf, irq.virq().raw());
    Ok((dev_kind, transport, irq))
}

#[cfg(not(target_arch = "x86_64"))]
fn fw_trigger_to_kind(t: InterruptTrigger) -> IrqTrigger {
    match t {
        InterruptTrigger::EdgeRising => IrqTrigger::EdgeRising,
        InterruptTrigger::EdgeFalling => IrqTrigger::EdgeFalling,
        InterruptTrigger::LevelHigh => IrqTrigger::LevelHigh,
        InterruptTrigger::LevelLow => IrqTrigger::LevelLow,
        InterruptTrigger::Unknown(f) => IrqTrigger::Unknown(f),
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn fw_controller_to_kind(c: InterruptControllerKind) -> IrqController {
    match c {
        InterruptControllerKind::Gic => IrqController::Gic,
        InterruptControllerKind::Plic => IrqController::Plic,
        InterruptControllerKind::Unknown => IrqController::Unknown,
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn legacy_irq_for_bdf(
    config: &PciConfigAccess,
    bdf: DeviceFunction,
    irq_provider: &'static dyn IrqOp,
) -> Option<IrqResource> {
    // Prefer firmware-described routing (device-tree / ACPI). This DT path
    // only carries controller-local hardware IRQs; the provider's `map_irq`
    // translates the (hwirq, trigger, controller) route into an OS-visible
    // virtual IRQ. ACPI GSI routes are resolved on the x86 path.
    if let Some(route) = legacy_interrupt_route(config, bdf)
        && let LegacyInterruptRoute::Hwirq {
            hwirq,
            trigger,
            controller,
        } = route
    {
        let desc = IrqRouteDesc {
            hwirq,
            trigger: fw_trigger_to_kind(trigger),
            controller: fw_controller_to_kind(controller),
            domain: None,
        };
        if let Ok(irq) = irq_provider.map_irq(desc) {
            return Some(irq);
        }
    }

    // Static fallback (no firmware routing): legacy PCI INTx ranges used by
    // each QEMU virt-style port. LoongArch PCH PIC starts at 0x10, RISC-V PLIC
    // PCI IRQs at 0x20, aarch64 test DT interrupt-map covers 0x20..0x23.
    cfg_select! {
        target_arch = "loongarch64" => {
            loongarch_legacy_irq(bdf.device, |desc| irq_provider.map_irq(desc))
        }
        target_arch = "aarch64" => {
            let desc = IrqRouteDesc {
                hwirq: 0x23 + (bdf.device & 3) as usize,
                trigger: IrqTrigger::LevelHigh,
                controller: IrqController::Gic,
                domain: None,
            };
            irq_provider.map_irq(desc).ok()
        }
        target_arch = "riscv64" => {
            let desc = IrqRouteDesc {
                hwirq: 0x20 + (bdf.device & 3) as usize,
                trigger: IrqTrigger::LevelHigh,
                controller: IrqController::Plic,
                domain: None,
            };
            irq_provider.map_irq(desc).ok()
        }
    }
}

#[cfg(any(target_arch = "loongarch64", unittest))]
fn loongarch_legacy_irq(
    device: u8,
    map_irq: impl FnOnce(IrqRouteDesc) -> driver_os_api::ResResult<IrqResource>,
) -> Option<IrqResource> {
    let desc = IrqRouteDesc {
        hwirq: 0x10 + usize::from(device & 3),
        trigger: IrqTrigger::LevelHigh,
        controller: IrqController::LoongArchExtioi,
        domain: None,
    };
    // Even an identity mapping must prepare the host's descriptor and flow.
    map_irq(desc).ok()
}

unittest::path_mod_test!(pci_irq_unittest, "../ut-tests/pci_irq_unittest.rs");
