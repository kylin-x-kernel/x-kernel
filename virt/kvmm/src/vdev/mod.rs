// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KVMM virtual-device interfaces and per-architecture device modules.
//!
//! Concrete vdevices live under `virt/vdev/*` crates. This module keeps the
//! world-switch hook traits that depend on KVMM's `Vcpu` type plus the shared
//! VM device registry. Architecture-specific devices live under per-arch
//! submodules such as [`aarch64`].

use alloc::{boxed::Box, sync::Arc, vec::Vec};

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod dma;
#[cfg(target_arch = "riscv64")]
pub mod riscv64;

pub use vdev_core::{
    GuestDma, IrqController, IrqSender, MmioBus, MmioDevice, RxChannel, VcpuWaker,
};

use crate::{arch::VmmArch, vcpu::Vcpu};

/// Per-vCPU world-switch hook.
pub trait VcpuHook<A: VmmArch>: Send {
    /// Called just before entering the guest (IRQs masked).
    fn on_entry(&mut self, vcpu: &mut Vcpu<A>);
    /// Called just after the guest exit is saved (IRQs still masked).
    fn on_exit(&mut self, vcpu_id: u32);
}

/// Factory for per-vCPU world-switch hooks provided by a VM device.
pub trait VcpuHookFactory<A: VmmArch>: Send + Sync {
    /// Create a hook instance for `vcpu_id`.
    fn make_vcpu_hook(&self, vcpu_id: u32) -> Option<Box<dyn VcpuHook<A>>>;
}

/// KVMM-owned virtual device registry.
pub struct VmDevices<A: VmmArch> {
    common: vdev_core::VmDevices<RxChannel>,
    hook_factories: ksync::Mutex<Vec<Arc<dyn VcpuHookFactory<A>>>>,
}

impl<A: VmmArch> VmDevices<A> {
    pub fn new() -> Self {
        Self {
            common: vdev_core::VmDevices::new(),
            hook_factories: ksync::Mutex::new(Vec::new()),
        }
    }

    pub fn mmio_bus(&self) -> &ksync::Mutex<MmioBus> {
        self.common.mmio_bus()
    }

    pub fn register_mmio(&self, dev: Box<dyn MmioDevice>) {
        self.common.register_mmio(dev);
    }

    pub fn mmio_ranges(&self) -> Vec<(alloc::string::String, u64, u64)> {
        self.common.mmio_ranges()
    }

    pub fn set_irq_controller(&self, irq_controller: Arc<dyn IrqController>) {
        self.common.set_irq_controller(irq_controller);
    }

    pub fn set_irq_sender(&self, irq_sender: Arc<dyn IrqSender>) {
        self.common.set_irq_sender(irq_sender);
    }

    pub fn inject_irq(&self, vcpu_id: u32, irq: u32) {
        self.common.inject_irq(vcpu_id, irq);
    }

    pub fn irq_sender(&self) -> Option<Arc<dyn IrqSender>> {
        self.common.irq_sender()
    }

    pub fn add_hook_factory(&self, hook_factory: Arc<dyn VcpuHookFactory<A>>) {
        self.hook_factories.lock().push(hook_factory);
    }

    pub fn install_vcpu_hooks(&self, vcpu: &mut Vcpu<A>) {
        for factory in self.hook_factories.lock().iter() {
            if let Some(hook) = factory.make_vcpu_hook(vcpu.vcpu_id) {
                vcpu.hooks.push(hook);
            }
        }
    }

    pub fn set_console_rx(&self, rx: Arc<RxChannel>) {
        self.common.set_console_rx(rx);
    }

    pub fn push_console(&self, byte: u8) -> bool {
        self.common
            .console_rx()
            .as_ref()
            .is_some_and(|rx| rx.push(byte))
    }

    pub fn device_names(&self) -> Vec<(alloc::string::String, u64)> {
        self.common.device_names()
    }
}

impl<A: VmmArch> Default for VmDevices<A> {
    fn default() -> Self {
        Self::new()
    }
}
