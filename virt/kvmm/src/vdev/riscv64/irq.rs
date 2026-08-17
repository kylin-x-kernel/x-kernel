// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simplified RISC-V virtual PLIC and interrupt injection.
//!
//! This follows the PLIC memory map for the priority, pending, enable,
//! threshold, and claim/complete registers, but intentionally only supports the
//! first 64 sources and maps PLIC context N directly to vCPU N.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::{
    arch::riscv64::{self, RiscvHext},
    vcpu::{MAX_VCPUS, Vcpu},
    vdev::{IrqController, IrqSender, MmioDevice, VcpuHook, VcpuHookFactory, VcpuWaker},
};

const MAX_IRQS: u32 = 64;
const VALID_IRQ_MASK: u64 = !1u64;

/// QEMU virt PLIC base and size.
pub const VPLIC_BASE: u64 = 0x0c00_0000;
pub const VPLIC_SIZE: u64 = 0x40_0000;

const PRIORITY_OFFSET: u64 = 0x000000;
const PENDING_OFFSET: u64 = 0x001000;
const ENABLE_OFFSET: u64 = 0x002000;
const ENABLE_STRIDE: u64 = 0x80;
const CONTEXT_OFFSET: u64 = 0x200000;
const CONTEXT_STRIDE: u64 = 0x1000;
const CONTEXT_THRESHOLD_OFFSET: u64 = 0x00;
const CONTEXT_CLAIM_COMPLETE_OFFSET: u64 = 0x04;

/// Per-VM minimal RISC-V interrupt controller.
pub struct RiscvIrq {
    nr_vcpus: usize,
    priority: [AtomicU32; MAX_IRQS as usize],
    pending: [AtomicU64; MAX_VCPUS],
    active: [AtomicU64; MAX_VCPUS],
    enable: [AtomicU64; MAX_VCPUS],
    threshold: [AtomicU32; MAX_VCPUS],
    waker: Weak<dyn VcpuWaker>,
}

impl RiscvIrq {
    pub fn new(nr_vcpus: usize, waker: Weak<dyn VcpuWaker>) -> Arc<Self> {
        Arc::new(Self {
            nr_vcpus,
            priority: core::array::from_fn(|_| AtomicU32::new(0)),
            pending: core::array::from_fn(|_| AtomicU64::new(0)),
            active: core::array::from_fn(|_| AtomicU64::new(0)),
            enable: core::array::from_fn(|_| AtomicU64::new(0)),
            threshold: core::array::from_fn(|_| AtomicU32::new(0)),
            waker,
        })
    }

    fn set_pending(&self, vcpu_id: u32, irq: u32) {
        if vcpu_id as usize >= self.nr_vcpus || irq >= MAX_IRQS {
            return;
        }
        self.pending[vcpu_id as usize].fetch_or(1u64 << irq, Ordering::Release);
        if let Some(waker) = self.waker.upgrade() {
            waker.wake_vcpu(vcpu_id);
        }
    }

    fn has_deliverable_irq(&self, vcpu_id: u32) -> bool {
        self.next_deliverable_irq(vcpu_id).is_some()
    }

    fn next_deliverable_irq(&self, vcpu_id: u32) -> Option<u32> {
        let context = vcpu_id as usize;
        if context >= self.nr_vcpus {
            return None;
        }

        let candidates = self.pending[context].load(Ordering::Acquire)
            & self.enable[context].load(Ordering::Acquire)
            & !self.active[context].load(Ordering::Acquire)
            & VALID_IRQ_MASK;
        let threshold = self.threshold[context].load(Ordering::Acquire);
        let mut best_irq = None;
        let mut best_prio = threshold;
        let mut pending = candidates;
        while pending != 0 {
            let irq = pending.trailing_zeros();
            pending &= pending - 1;
            let prio = self.priority[irq as usize].load(Ordering::Acquire);
            if prio > best_prio {
                best_irq = Some(irq);
                best_prio = prio;
            }
        }
        best_irq
    }

    fn claim(&self, vcpu_id: u32) -> u32 {
        let context = vcpu_id as usize;
        let Some(irq) = self.next_deliverable_irq(vcpu_id) else {
            return 0;
        };
        let mask = 1u64 << irq;
        self.pending[context].fetch_and(!mask, Ordering::AcqRel);
        self.active[context].fetch_or(mask, Ordering::AcqRel);
        irq
    }

    fn complete(&self, vcpu_id: u32, irq: u32) {
        let context = vcpu_id as usize;
        if context >= self.nr_vcpus || irq == 0 || irq >= MAX_IRQS {
            return;
        }
        self.active[context].fetch_and(!(1u64 << irq), Ordering::AcqRel);
    }

    fn read_pending_word(&self, word: usize) -> u32 {
        if word >= 2 {
            return 0;
        }
        let mut pending = 0u64;
        for context in 0..self.nr_vcpus {
            pending |= self.pending[context].load(Ordering::Acquire);
        }
        ((pending >> (word * 32)) & u32::MAX as u64) as u32
    }

    fn read_enable_word(&self, context: usize, word: usize) -> u32 {
        if context >= self.nr_vcpus || word >= 2 {
            return 0;
        }
        ((self.enable[context].load(Ordering::Acquire) >> (word * 32)) & u32::MAX as u64) as u32
    }

    fn write_enable_word(&self, context: usize, word: usize, val: u32) {
        if context >= self.nr_vcpus || word >= 2 {
            return;
        }
        let shift = word * 32;
        let word_mask = (u32::MAX as u64) << shift;
        let val = ((val as u64) << shift) & word_mask & VALID_IRQ_MASK;
        let enable = &self.enable[context];
        let mut old = enable.load(Ordering::Acquire);
        loop {
            let new = (old & !word_mask) | val;
            match enable.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(current) => old = current,
            }
        }
    }

    fn read(&self, offset: u64) -> u32 {
        match offset {
            PRIORITY_OFFSET..PENDING_OFFSET => {
                let irq = (offset / 4) as usize;
                if irq < MAX_IRQS as usize {
                    self.priority[irq].load(Ordering::Acquire)
                } else {
                    0
                }
            }
            PENDING_OFFSET..ENABLE_OFFSET => {
                self.read_pending_word(((offset - PENDING_OFFSET) / 4) as usize)
            }
            ENABLE_OFFSET..CONTEXT_OFFSET => {
                let rel = offset - ENABLE_OFFSET;
                self.read_enable_word(
                    (rel / ENABLE_STRIDE) as usize,
                    ((rel % ENABLE_STRIDE) / 4) as usize,
                )
            }
            offset if offset >= CONTEXT_OFFSET => {
                let rel = offset - CONTEXT_OFFSET;
                let context = (rel / CONTEXT_STRIDE) as usize;
                match rel % CONTEXT_STRIDE {
                    CONTEXT_THRESHOLD_OFFSET => self
                        .threshold
                        .get(context)
                        .map_or(0, |threshold| threshold.load(Ordering::Acquire)),
                    CONTEXT_CLAIM_COMPLETE_OFFSET => self.claim(context as u32),
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write(&self, offset: u64, val: u32) {
        match offset {
            PRIORITY_OFFSET..PENDING_OFFSET => {
                let irq = (offset / 4) as usize;
                if irq > 0 && irq < MAX_IRQS as usize {
                    self.priority[irq].store(val, Ordering::Release);
                }
            }
            PENDING_OFFSET..ENABLE_OFFSET => {
                let word = ((offset - PENDING_OFFSET) / 4) as usize;
                if word < 2 {
                    let pending = ((val as u64) << (word * 32)) & VALID_IRQ_MASK;
                    for context in 0..self.nr_vcpus {
                        self.pending[context].fetch_or(pending, Ordering::Release);
                    }
                }
            }
            ENABLE_OFFSET..CONTEXT_OFFSET => {
                let rel = offset - ENABLE_OFFSET;
                self.write_enable_word(
                    (rel / ENABLE_STRIDE) as usize,
                    ((rel % ENABLE_STRIDE) / 4) as usize,
                    val,
                );
            }
            offset if offset >= CONTEXT_OFFSET => {
                let rel = offset - CONTEXT_OFFSET;
                let context = (rel / CONTEXT_STRIDE) as usize;
                match rel % CONTEXT_STRIDE {
                    CONTEXT_THRESHOLD_OFFSET => {
                        if let Some(threshold) = self.threshold.get(context) {
                            threshold.store(val, Ordering::Release);
                        }
                    }
                    CONTEXT_CLAIM_COMPLETE_OFFSET => self.complete(context as u32, val),
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// MMIO front-end for [`RiscvIrq`].
pub struct RiscvPlicMmio {
    irq: Arc<RiscvIrq>,
}

impl RiscvPlicMmio {
    pub fn new(irq: Arc<RiscvIrq>) -> Self {
        Self { irq }
    }
}

impl MmioDevice for RiscvPlicMmio {
    fn name(&self) -> &str {
        "riscv-vplic"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (VPLIC_BASE, VPLIC_SIZE)
    }

    fn read(&self, offset: u64, size: u8) -> u64 {
        if size != 4 {
            return 0;
        }
        self.irq.read(offset) as u64
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        if size == 4 {
            self.irq.write(offset, value as u32);
        }
    }
}

impl IrqSender for RiscvIrq {
    fn inject(&self, vcpu_id: u32, irq: u32) {
        self.set_pending(vcpu_id, irq);
    }
}

impl IrqController for RiscvIrq {
    fn inject_irq(&self, vcpu_id: u32, irq: u32) {
        self.set_pending(vcpu_id, irq);
    }
}

/// Hook factory for minimal RISC-V interrupt injection.
pub struct RiscvIrqHookFactory {
    irq: Arc<RiscvIrq>,
}

impl RiscvIrqHookFactory {
    pub fn new(irq: Arc<RiscvIrq>) -> Self {
        Self { irq }
    }
}

impl VcpuHookFactory<RiscvHext> for RiscvIrqHookFactory {
    fn make_vcpu_hook(&self, _vcpu_id: u32) -> Option<alloc::boxed::Box<dyn VcpuHook<RiscvHext>>> {
        Some(alloc::boxed::Box::new(RiscvIrqHook {
            irq: self.irq.clone(),
        }))
    }
}

struct RiscvIrqHook {
    irq: Arc<RiscvIrq>,
}

impl VcpuHook<RiscvHext> for RiscvIrqHook {
    fn on_entry(&mut self, vcpu: &mut Vcpu<RiscvHext>) {
        riscv64::set_virtual_external_irq_pending(self.irq.has_deliverable_irq(vcpu.vcpu_id));
    }

    fn on_exit(&mut self, _vcpu_id: u32) {
        riscv64::set_virtual_external_irq_pending(false);
    }
}
