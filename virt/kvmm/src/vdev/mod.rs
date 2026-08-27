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
use core::sync::atomic::{AtomicU32, Ordering};

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod dma;
#[cfg(target_arch = "riscv64")]
pub mod riscv64;

pub use vdev_core::{
    GuestDma, IrqController, IrqSender, MmioBus, MmioDevice, RxChannel, TxChannel, VcpuWaker,
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

/// Sentinel for "no console interrupt wired": [`VmDevices::push_console`]
/// injects nothing.
const NO_CONSOLE_IRQ: u32 = u32::MAX;

/// KVMM-owned virtual device registry.
pub struct VmDevices<A: VmmArch> {
    common: vdev_core::VmDevices<RxChannel, TxChannel>,
    hook_factories: ksync::Mutex<Vec<Arc<dyn VcpuHookFactory<A>>>>,
    /// Controller line raised when console RX data arrives, or
    /// [`NO_CONSOLE_IRQ`] to leave the console in polled mode.
    console_irq: AtomicU32,
}

impl<A: VmmArch> VmDevices<A> {
    pub fn new() -> Self {
        Self {
            common: vdev_core::VmDevices::new(),
            hook_factories: ksync::Mutex::new(Vec::new()),
            console_irq: AtomicU32::new(NO_CONSOLE_IRQ),
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

    /// Install the guest→host console TX channel and switch the UART into
    /// channel mode (guest output is forwarded to the channel instead of the
    /// host kernel log). The channel is drained by the owning control device's
    /// `read` path; see [`drain_console`](Self::drain_console).
    pub fn set_console_tx(&self, tx: Arc<TxChannel>) {
        tx.set_enabled(true);
        self.common.set_console_tx(tx);
    }

    /// True if the guest has produced console output not yet drained.
    pub fn console_has_output(&self) -> bool {
        self.common
            .console_tx()
            .map(|tx| tx.has_data())
            .unwrap_or(false)
    }

    /// Drain pending guest console output into `buf`, returning the byte count.
    /// Returns 0 when no channel is installed or nothing is pending.
    pub fn drain_console(&self, buf: &mut [u8]) -> usize {
        self.common
            .console_tx()
            .map(|tx| tx.drain(buf))
            .unwrap_or(0)
    }

    /// Route console RX interrupts to controller line `irq` on vCPU 0.
    ///
    /// Only meaningful once an [`IrqController`] is installed. Leaving it unset
    /// keeps the console in polled mode (the previous behaviour).
    pub fn set_console_irq(&self, irq: u32) {
        self.console_irq.store(irq, Ordering::Release);
    }

    /// Push one byte into the guest console RX FIFO.
    ///
    /// On success, and when a console interrupt is wired, raise it on vCPU 0
    /// if the guest has enabled RX interrupts. Injecting through the interrupt
    /// controller also wakes a vCPU parked in WFI so typed input is delivered
    /// promptly rather than only on the next unrelated guest exit.
    pub fn push_console(&self, byte: u8) -> bool {
        let Some(rx) = self.common.console_rx() else {
            return false;
        };
        if !rx.push(byte) {
            return false;
        }
        let irq = self.console_irq.load(Ordering::Acquire);
        if irq != NO_CONSOLE_IRQ && rx.irq_pending() {
            self.common.inject_irq(0, irq);
        }
        true
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
