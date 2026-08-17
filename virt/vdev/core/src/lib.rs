// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Virtual device interfaces shared by VMMs and vdevice crates.

#![no_std]

extern crate alloc;

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};

/// A single MMIO device that handles reads and writes at a fixed guest-physical range.
pub trait MmioDevice: Send {
    /// Human-readable device name.
    fn name(&self) -> &str;

    /// Returns `(base_gpa, size)` for this device's MMIO window.
    fn mmio_range(&self) -> (u64, u64);

    /// Handle an MMIO read.
    fn read(&self, offset: u64, size: u8) -> u64;

    /// Handle an MMIO read with the accessing vCPU id.
    fn read_for_vcpu(&self, offset: u64, size: u8, _vcpu_id: u32) -> u64 {
        self.read(offset, size)
    }

    /// Handle an MMIO write.
    fn write(&mut self, offset: u64, size: u8, value: u64);

    /// Handle an MMIO write with the accessing vCPU id.
    fn write_for_vcpu(&mut self, offset: u64, size: u8, value: u64, _vcpu_id: u32) {
        self.write(offset, size, value);
    }
}

/// MMIO bus that dispatches guest memory-mapped I/O to registered devices.
pub struct MmioBus {
    devices: Vec<Box<dyn MmioDevice>>,
}

impl MmioBus {
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    pub fn register(&mut self, dev: Box<dyn MmioDevice>) {
        self.devices.push(dev);
    }

    pub fn handle(&mut self, gpa: u64, is_write: bool, size: u8, value: u64) -> Option<u64> {
        self.handle_for_vcpu(gpa, is_write, size, value, 0)
    }

    pub fn handle_for_vcpu(
        &mut self,
        gpa: u64,
        is_write: bool,
        size: u8,
        value: u64,
        vcpu_id: u32,
    ) -> Option<u64> {
        for dev in &mut self.devices {
            let (base, dev_size) = dev.mmio_range();
            if gpa >= base && gpa < base + dev_size {
                let offset = gpa - base;
                return if is_write {
                    dev.write_for_vcpu(offset, size, value, vcpu_id);
                    Some(0)
                } else {
                    Some(dev.read_for_vcpu(offset, size, vcpu_id))
                };
            }
        }
        None
    }

    pub fn device_list(&self) -> Vec<(String, u64)> {
        self.devices
            .iter()
            .map(|d| {
                let (base, _) = d.mmio_range();
                (String::from(d.name()), base)
            })
            .collect()
    }

    pub fn device_ranges(&self) -> Vec<(String, u64, u64)> {
        self.devices
            .iter()
            .map(|d| {
                let (base, size) = d.mmio_range();
                (String::from(d.name()), base, size)
            })
            .collect()
    }
}

impl Default for MmioBus {
    fn default() -> Self {
        Self::new()
    }
}

/// A source that can raise a virtual interrupt into a target vCPU.
pub trait IrqSender: Send + Sync {
    fn inject(&self, vcpu_id: u32, irq: u32);
}

/// Virtual interrupt controller capability owned by a VM.
pub trait IrqController: Send + Sync {
    fn inject_irq(&self, vcpu_id: u32, irq: u32);
}

/// VM-owned wakeup hook used by interrupt controllers after queueing IRQs.
pub trait VcpuWaker: Send + Sync {
    fn wake_vcpu(&self, vcpu_id: u32);
}

/// Guest physical memory access for virtual device backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    NoGuestMem,
    AddressFault,
    RangeOverflow,
}

pub trait GuestDma: Send + Sync {
    fn read(&self, gpa: u64, buf: &mut [u8]) -> Result<(), DmaError>;
    fn write(&self, gpa: u64, buf: &[u8]) -> Result<(), DmaError>;

    fn read_u16(&self, gpa: u64) -> Result<u16, DmaError> {
        let mut buf = [0u8; 2];
        self.read(gpa, &mut buf)?;
        Ok(u16::from_le_bytes(buf))
    }

    fn read_u32(&self, gpa: u64) -> Result<u32, DmaError> {
        let mut buf = [0u8; 4];
        self.read(gpa, &mut buf)?;
        Ok(u32::from_le_bytes(buf))
    }

    fn read_u64(&self, gpa: u64) -> Result<u64, DmaError> {
        let mut buf = [0u8; 8];
        self.read(gpa, &mut buf)?;
        Ok(u64::from_le_bytes(buf))
    }

    fn write_u16(&self, gpa: u64, val: u16) -> Result<(), DmaError> {
        self.write(gpa, &val.to_le_bytes())
    }

    fn write_u32(&self, gpa: u64, val: u32) -> Result<(), DmaError> {
        self.write(gpa, &val.to_le_bytes())
    }
}

/// VM-owned virtual device capabilities.
pub struct VmDevices<Rx> {
    mmio_bus: ksync::Mutex<MmioBus>,
    irq_controller: ksync::Mutex<Option<Arc<dyn IrqController>>>,
    irq_sender: ksync::Mutex<Option<Arc<dyn IrqSender>>>,
    console_rx: ksync::Mutex<Option<Arc<Rx>>>,
}

impl<Rx> VmDevices<Rx> {
    pub fn new() -> Self {
        Self {
            mmio_bus: ksync::Mutex::new(MmioBus::new()),
            irq_controller: ksync::Mutex::new(None),
            irq_sender: ksync::Mutex::new(None),
            console_rx: ksync::Mutex::new(None),
        }
    }

    pub fn mmio_bus(&self) -> &ksync::Mutex<MmioBus> {
        &self.mmio_bus
    }

    pub fn register_mmio(&self, dev: Box<dyn MmioDevice>) {
        self.mmio_bus.lock().register(dev);
    }

    pub fn mmio_ranges(&self) -> Vec<(String, u64, u64)> {
        self.mmio_bus.lock().device_ranges()
    }

    pub fn set_irq_controller(&self, irq_controller: Arc<dyn IrqController>) {
        *self.irq_controller.lock() = Some(irq_controller);
    }

    pub fn set_irq_sender(&self, irq_sender: Arc<dyn IrqSender>) {
        *self.irq_sender.lock() = Some(irq_sender);
    }

    pub fn inject_irq(&self, vcpu_id: u32, irq: u32) {
        if let Some(irq_controller) = self.irq_controller.lock().clone() {
            irq_controller.inject_irq(vcpu_id, irq);
        }
    }

    pub fn irq_sender(&self) -> Option<Arc<dyn IrqSender>> {
        self.irq_sender.lock().clone()
    }

    pub fn set_console_rx(&self, rx: Arc<Rx>) {
        *self.console_rx.lock() = Some(rx);
    }

    pub fn console_rx(&self) -> Option<Arc<Rx>> {
        self.console_rx.lock().clone()
    }

    pub fn device_names(&self) -> Vec<(String, u64)> {
        self.mmio_bus.lock().device_list()
    }
}

impl<Rx> Default for VmDevices<Rx> {
    fn default() -> Self {
        Self::new()
    }
}
