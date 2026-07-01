// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO PCI transport integration.

#[cfg(not(target_arch = "x86_64"))]
use device_res::{IrqController, IrqRouteDesc, IrqTriggerMode, irq_provider};
use driver_base::DeviceKind;
use pci::PciConfigAccess;
#[cfg(not(target_arch = "x86_64"))]
use pci::legacy_interrupt_route;
#[cfg(not(target_arch = "x86_64"))]
use pci::{InterruptControllerKind, InterruptTrigger};
#[cfg(target_arch = "x86_64")]
mod x86_64;
use virtio_drivers::{
    PhysAddr, Result as VirtIoResult,
    transport::{
        DeviceStatus, DeviceType, InterruptStatus, Transport,
        pci::{
            PciTransport as RawPciTransport, VirtioPciError,
            bus::{ConfigurationAccess, DeviceFunction, DeviceFunctionInfo, PciRoot},
            virtio_device_type,
        },
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

#[cfg(target_arch = "x86_64")]
use self::x86_64::{PciIrqState, PciMsixState, legacy_irq_for_bdf, setup_msix};
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
        #[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))] msix: PciMsixStateOption,
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

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_pci_device<H: VirtIoHal, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
    config: &mut PciConfigAccess,
) -> Option<(DeviceKind, PciTransport, usize)> {
    let dev_kind = virtio_device_type(dev_info).and_then(as_device_kind)?;

    #[cfg(target_arch = "x86_64")]
    let (irq, msix) = {
        // MSI-X setup is limited to network devices for this x86 virtio-net
        // latency fix. Other virtio device kinds keep legacy IRQs until their
        // interrupt behavior is audited with the shared queue-vector setup.
        if dev_kind == DeviceKind::Net {
            setup_msix::<H, C>(root, bdf, config)
                .map(|(irq, msix)| (irq, Some(msix)))
                .or_else(|| legacy_irq_for_bdf(config, bdf).map(|irq| (irq, None)))?
        } else {
            (legacy_irq_for_bdf(config, bdf)?, None)
        }
    };

    #[cfg(not(target_arch = "x86_64"))]
    let irq = { legacy_irq_for_bdf(config, bdf)? };

    #[cfg(not(target_arch = "x86_64"))]
    let msix = None;

    let transport = PciTransport::new_with_msix::<H, C>(root, bdf, msix).ok()?;
    log::info!("PCI virtio device at {:?}: IRQ = {}", bdf, irq);
    Some((dev_kind, transport, irq))
}

#[cfg(not(target_arch = "x86_64"))]
fn fw_trigger_to_mode(t: InterruptTrigger) -> IrqTriggerMode {
    match t {
        InterruptTrigger::EdgeRising => IrqTriggerMode::EdgeRising,
        InterruptTrigger::EdgeFalling => IrqTriggerMode::EdgeFalling,
        InterruptTrigger::LevelHigh => IrqTriggerMode::LevelHigh,
        InterruptTrigger::LevelLow => IrqTriggerMode::LevelLow,
        InterruptTrigger::Unknown(_) => IrqTriggerMode::Unspecified,
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
fn legacy_irq_for_bdf(config: &PciConfigAccess, bdf: DeviceFunction) -> Option<usize> {
    // Prefer firmware-described routing (device-tree / ACPI). The provider's
    // `map_irq` translates the (hwirq, trigger, controller) route into an
    // OS-visible virtual IRQ, replacing the per-architecture `khal::irq`
    // descriptor constructors.
    if let Some(route) = legacy_interrupt_route(config, bdf) {
        let desc = IrqRouteDesc {
            hwirq: route.irq,
            trigger: fw_trigger_to_mode(route.trigger),
            controller: fw_controller_to_kind(route.controller),
            domain: None,
        };
        if let Ok(p) = irq_provider()
            && let Ok(irq) = p.map_irq(desc)
        {
            return Some(irq.number);
        }
    }

    // Static fallback (no firmware routing): legacy PCI INTx ranges used by
    // each QEMU virt-style port. LoongArch PCH PIC starts at 0x10, RISC-V PLIC
    // PCI IRQs at 0x20, aarch64 test DT interrupt-map covers 0x20..0x23.
    cfg_select! {
        target_arch = "loongarch64" => {
            // LoongArch uses a 1:1 hwirq→virq mapping for these lines.
            Some(0x10 + (bdf.device & 3) as usize)
        }
        target_arch = "aarch64" => {
            let desc = IrqRouteDesc {
                hwirq: 0x23 + (bdf.device & 3) as usize,
                trigger: IrqTriggerMode::LevelHigh,
                controller: IrqController::Gic,
                domain: None,
            };
            Some(irq_provider().ok()?.map_irq(desc).ok()?.number)
        }
        target_arch = "riscv64" => {
            let desc = IrqRouteDesc {
                hwirq: 0x20 + (bdf.device & 3) as usize,
                trigger: IrqTriggerMode::LevelHigh,
                controller: IrqController::Plic,
                domain: None,
            };
            Some(irq_provider().ok()?.map_irq(desc).ok()?.number)
        }
    }
}
