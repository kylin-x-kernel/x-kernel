// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal GICv2 distributor (GICD) emulation.
//!
//! The guest programs the distributor to enable/prioritise interrupts; those
//! MMIO accesses trap (GICD is never mapped into the guest — see
//! [`crate::mm::stage2`]) and land here. This is a bring-up-grade model:
//!
//! - a byte-addressable backing store gives correct read-back for the registers
//!   the guest probes (IPRIORITYR / ITARGETSR / ICFGR / CTLR) — without this the
//!   guest's "how many priority bits?" probe reads 0 and hangs;
//! - enable state (ISENABLER/ICENABLER) is tracked as a bitmap;
//! - guest-set-pending (ISPENDR) and SGI (SGIR) writes are turned into
//!   injections via the [`IrqSender`] (the vGIC).
//!
//! The real host distributor is never touched.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use super::vgic::Vgic;
use crate::mm::mmio::MmioDevice;

/// QEMU virt GICv2 distributor base / size.
pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICD_SIZE: u64 = 0x1_0000;

/// Size of the backing store (covers CTLR .. ICFGR; guest never probes above).
const REG_SIZE: usize = 0x1000;
const ENABLE_WORDS: usize = 32; // 32 * 32 = 1024 IRQs

const GICD_CTLR: u64 = 0x000;
const GICD_TYPER: u64 = 0x004;
const GICD_IIDR: u64 = 0x008;
const GICD_ISENABLER: u64 = 0x100; // .. 0x17c
const GICD_ICENABLER: u64 = 0x180; // .. 0x1fc
const GICD_ISPENDR: u64 = 0x200; // .. 0x27c
const GICD_ICPENDR: u64 = 0x280; // .. 0x2fc
const GICD_ITARGETSR: u64 = 0x800; // .. 0xbfc (0x800..0x820 banked, read-only)
const GICD_SGIR: u64 = 0xf00;
const GUEST_VTIMER_IRQ: u32 = 27;
static SGIR_LOG_COUNT: AtomicU64 = AtomicU64::new(0);

fn set_host_vtimer_irq_enabled(enabled: bool) {
    super::vtimer::set_host_vtimer_irq_enabled(enabled);
}

fn word_index(off: u64, base: u64) -> Option<usize> {
    if off >= base && off < base + (ENABLE_WORDS as u64) * 4 {
        Some(((off - base) / 4) as usize)
    } else {
        None
    }
}

/// Emulated GIC distributor for one VM.
pub struct Vgicd {
    /// Byte-addressable register backing store (read-back correctness).
    regs: Box<[u8; REG_SIZE]>,
    /// Per-vCPU GICC/GICH state and pending injection backend.
    vgic: Arc<Vgic>,
    nr_vcpus: usize,
}

impl Vgicd {
    pub fn new(vgic: Arc<Vgic>, nr_vcpus: usize) -> Self {
        Self {
            regs: Box::new([0u8; REG_SIZE]),
            vgic,
            nr_vcpus,
        }
    }

    fn rd(&self, off: usize, size: u8) -> u64 {
        let mut v = 0u64;
        for i in 0..size as usize {
            if off + i < REG_SIZE {
                v |= (self.regs[off + i] as u64) << (8 * i);
            }
        }
        v
    }

    fn wr(&mut self, off: usize, size: u8, val: u64) {
        for i in 0..size as usize {
            if off + i < REG_SIZE {
                self.regs[off + i] = (val >> (8 * i)) as u8;
            }
        }
    }

    fn inject_to_mask(&self, irq: u32, cpu_mask: u32, source_vcpu: u32) {
        for cpu in 0..self.nr_vcpus.min(32) {
            if cpu_mask & (1 << cpu) != 0 {
                self.vgic.set_pending_from(cpu as u32, irq, source_vcpu);
            }
        }
    }

    fn target_mask_for_irq(&self, irq: u32, source_vcpu: u32) -> u32 {
        if irq < 32 {
            return 1 << source_vcpu;
        }
        let off = GICD_ITARGETSR as usize + irq as usize;
        let mask = if off < REG_SIZE {
            self.regs[off] as u32
        } else {
            0
        };
        if mask == 0 { 1 } else { mask }
    }

    fn handle_sgir(&self, value: u32, source_vcpu: u32) {
        let irq = value & 0xf;
        let filter = (value >> 24) & 0x3;
        let mask = match filter {
            0 => (value >> 16) & 0xff,
            1 => ((1u32 << self.nr_vcpus.min(32)) - 1) & !(1 << source_vcpu),
            2 => 1 << source_vcpu,
            _ => 0,
        };
        let n = SGIR_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
        if n < 128 || n.is_power_of_two() {
            log::info!(
                "[vgicd] SGIR#{n}: src={} irq={} filter={} raw_mask={:#x} target_mask={:#x} \
                 value={:#x}",
                source_vcpu,
                irq,
                filter,
                (value >> 16) & 0xff,
                mask,
                value,
            );
        }
        self.inject_to_mask(irq, mask, source_vcpu);
    }
}

impl MmioDevice for Vgicd {
    fn name(&self) -> &str {
        "vgicd"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (GICD_BASE, GICD_SIZE)
    }

    fn read(&self, offset: u64, size: u8) -> u64 {
        self.read_for_vcpu(offset, size, 0)
    }

    fn read_for_vcpu(&self, offset: u64, size: u8, vcpu_id: u32) -> u64 {
        match offset {
            GICD_TYPER => (((self.nr_vcpus.saturating_sub(1)) as u64) << 5) | 0x3,
            GICD_IIDR => 0x0000_0000,
            o if word_index(o, GICD_ISENABLER).is_some() => self
                .vgic
                .enabled_word(vcpu_id, word_index(o, GICD_ISENABLER).unwrap())
                as u64,
            o if word_index(o, GICD_ICENABLER).is_some() => self
                .vgic
                .enabled_word(vcpu_id, word_index(o, GICD_ICENABLER).unwrap())
                as u64,
            // ITARGETSR for SGIs/PPIs (IRQ 0-31) is banked read-only. Linux
            // uses this to discover each CPU's GIC target mask for SGI/IPI.
            o if (GICD_ITARGETSR..GICD_ITARGETSR + 0x20).contains(&o) => {
                let mask = 1u32 << vcpu_id.min(31);
                u64::from(mask | (mask << 8) | (mask << 16) | (mask << 24))
            }
            o => self.rd(o as usize, size),
        }
    }

    fn write(&mut self, offset: u64, size: u8, value: u64) {
        self.write_for_vcpu(offset, size, value, 0)
    }

    fn write_for_vcpu(&mut self, offset: u64, size: u8, value: u64, vcpu_id: u32) {
        let v = value as u32;
        match offset {
            GICD_CTLR => self.wr(GICD_CTLR as usize, size, value),
            o if word_index(o, GICD_ISENABLER).is_some() => {
                let word = word_index(o, GICD_ISENABLER).unwrap();
                for bit in 0..32 {
                    if v & (1 << bit) != 0 {
                        self.vgic.set_enabled(vcpu_id, word as u32 * 32 + bit, true);
                    }
                }
                if word == 0 && v & (1 << GUEST_VTIMER_IRQ) != 0 {
                    set_host_vtimer_irq_enabled(true);
                }
            }
            o if word_index(o, GICD_ICENABLER).is_some() => {
                let word = word_index(o, GICD_ICENABLER).unwrap();
                for bit in 0..32 {
                    if v & (1 << bit) != 0 {
                        self.vgic
                            .set_enabled(vcpu_id, word as u32 * 32 + bit, false);
                    }
                }
                if word == 0 && v & (1 << GUEST_VTIMER_IRQ) != 0 {
                    set_host_vtimer_irq_enabled(false);
                }
            }
            o if word_index(o, GICD_ISPENDR).is_some() => {
                let word = word_index(o, GICD_ISPENDR).unwrap() as u32;
                for b in 0..32 {
                    if v & (1 << b) != 0 {
                        let irq = word * 32 + b;
                        let mask = self.target_mask_for_irq(irq, vcpu_id);
                        self.inject_to_mask(irq, mask, vcpu_id);
                    }
                }
            }
            o if word_index(o, GICD_ICPENDR).is_some() => {
                // Clearing pending is best-effort in this model; ignore.
            }
            GICD_SGIR => {
                self.handle_sgir(v, vcpu_id);
            }
            // IPRIORITYR / ITARGETSR (SPIs) / ICFGR / etc: store for read-back.
            o => self.wr(o as usize, size, value),
        }
    }
}
