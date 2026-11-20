// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Owned device and PCI state for system sleep, prepared before task freezing.

use alloc::{sync::Arc, vec::Vec};

use driver_base::{DriverError, DriverResult};
use kdevice::{BindingRef, DeviceIdentity, DeviceLocation, DeviceObject, DriverObject};
use pci::{Cam, DeviceFunction, PciBus, PciConfigAccess};

/// Returns the boot-discovered ACPI S3 package without evaluating AML in PM.
#[cfg(target_arch = "x86_64")]
pub fn s3_sleep_types() -> Option<[u8; 2]> {
    crate::bus::acpi_owner::s3_sleep_types()
}

struct PciState {
    bdf: DeviceFunction,
    bars: [u32; 6],
    command: u32,
    line: u32,
}

struct Participant {
    device: Arc<DeviceObject>,
    driver: Arc<DriverObject>,
    binding: BindingRef,
    depth: usize,
}

/// Retains the exact bound participants through suspend, rollback and recovery.
/// The caller must freeze device producers and discovery/removal before suspend,
/// keep IRQ routing masked through recovery, and serialize system transactions.
pub struct PowerDevices {
    devices: Vec<Participant>,
    pci: Vec<PciState>,
    config: Option<PciConfigAccess>,
    suspended: usize,
    pci_suspended: usize,
}

impl PowerDevices {
    /// Captures bound identities and PCI state without powering anything down.
    /// Unknown driver PM contracts fail before any callback or hardware change.
    /// # Errors
    /// Returns Unsupported for an unimplemented driver or PCI bridge context;
    /// malformed or disappearing topology returns BadState.
    pub fn prepare() -> DriverResult<Self> {
        // Unmatched discovery descriptors have no DeviceObject/record. Firmware
        // can still execute their VGA option ROMs, so validate discovery rather
        // than only the bound runtime participants before any power change.
        if kdevice::device_descs_snapshot().iter().any(|desc| {
            matches!(desc.identity(), DeviceIdentity::Pci(identity) if identity.class == 0x03)
        }) {
            log::warn!("system sleep requires a display-free PCI profile");
            return Err(DriverError::Unsupported);
        }
        let records = kdevice::device_records_snapshot();
        let mut devices = Vec::new();
        let mut pci_states = Vec::new();
        let mut config = None;
        for record in &records {
            let Some(driver_id) = record.driver_id else {
                continue;
            };
            let device = kdevice::find_device(record.id).ok_or(DriverError::BadState)?;
            let driver = kdevice::find_driver(driver_id).ok_or(DriverError::BadState)?;
            let binding = device.binding_ref().ok_or(DriverError::BadState)?;
            if binding.driver_id() != driver_id {
                return Err(DriverError::BadState);
            }
            if !driver.supports_suspend() {
                log::warn!("system sleep unsupported by driver {}", driver.name());
                return Err(DriverError::Unsupported);
            }
            let mut depth = 0;
            let mut parent = record.parent;
            while let Some(id) = parent {
                depth += 1;
                if depth > records.len() {
                    return Err(DriverError::BadState);
                }
                parent = records
                    .iter()
                    .find(|item| item.id == id)
                    .ok_or(DriverError::BadState)?
                    .parent;
            }
            if let DeviceLocation::Pci {
                segment,
                bus,
                device,
                function,
            } = record.location
            {
                if segment != 0 {
                    return Err(DriverError::Unsupported);
                }
                if config.is_none() {
                    let mut bus = PciBus::new(Cam::Ecam).map_err(|_| DriverError::Io)?;
                    config = Some(*bus.parts_mut().1);
                }
                let access = config.as_ref().ok_or(DriverError::BadState)?;
                let bdf = DeviceFunction {
                    bus,
                    device,
                    function,
                };
                let read = |offset, width| {
                    access
                        .try_read(bdf, offset, width)
                        .map_err(|_| DriverError::Io)
                };
                if read(0x0e, 1)? & 0x7f != 0 {
                    return Err(DriverError::Unsupported);
                }
                let mut bars = [0; 6];
                for (index, bar) in bars.iter_mut().enumerate() {
                    *bar = read(0x10 + index as u16 * 4, 4)?;
                }
                pci_states.push(PciState {
                    bdf,
                    bars,
                    command: read(4, 2)?,
                    line: read(0x3c, 1)?,
                });
            }
            devices.push(Participant {
                device,
                driver,
                binding,
                depth,
            });
        }
        devices.sort_by_key(|participant| core::cmp::Reverse(participant.depth));
        Ok(Self {
            devices,
            pci: pci_states,
            config,
            suspended: 0,
            pci_suspended: 0,
        })
    }

    /// Quiesces consumer devices before providers, then disables PCI bus DMA.
    /// Busy callbacks leave their own device operational; completed predecessors
    /// remain retained for explicit rollback before any producer is released.
    /// # Errors
    /// Propagates the failing driver or config-space error.
    pub fn suspend(&mut self) -> DriverResult<()> {
        for participant in &self.devices {
            participant.device.power_transition(
                &participant.binding,
                &participant.driver,
                false,
            )?;
            self.suspended += 1;
        }
        if let Some(config) = &self.config {
            for state in &self.pci {
                config
                    .try_write(state.bdf, 4, 2, state.command & !4)
                    .map_err(|_| DriverError::Io)?;
                self.pci_suspended += 1;
            }
        }
        Ok(())
    }

    /// Reconstructs BAR/decode state, devices, and finally bus-master admission.
    /// IRQ routing and frozen producers must remain excluded until completion.
    /// # Errors
    /// Failure retains the remaining suspended prefix; do not thaw producers.
    pub fn resume(&mut self) -> DriverResult<()> {
        if let Some(config) = &self.config {
            for state in &self.pci[..self.pci_suspended] {
                config
                    .try_write(state.bdf, 4, 2, 0)
                    .map_err(|_| DriverError::Io)?;
                for (index, bar) in state.bars.iter().enumerate() {
                    config
                        .try_write(state.bdf, 0x10 + index as u16 * 4, 4, *bar)
                        .map_err(|_| DriverError::Io)?;
                }
                config
                    .try_write(state.bdf, 0x3c, 1, state.line)
                    .map_err(|_| DriverError::Io)?;
                config
                    .try_write(state.bdf, 4, 2, state.command & !4)
                    .map_err(|_| DriverError::Io)?;
            }
        }
        while self.suspended != 0 {
            let participant = &self.devices[self.suspended - 1];
            participant
                .device
                .power_transition(&participant.binding, &participant.driver, true)?;
            self.suspended -= 1;
        }
        if let Some(config) = &self.config {
            for state in &self.pci[..self.pci_suspended] {
                config
                    .try_write(state.bdf, 4, 2, state.command)
                    .map_err(|_| DriverError::Io)?;
            }
        }
        self.pci_suspended = 0;
        Ok(())
    }
}

unittest::path_mod_test!(tests, "../ut-tests/power.rs");
