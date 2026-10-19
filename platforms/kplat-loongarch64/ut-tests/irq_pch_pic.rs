// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

/// A recorded register access against the recording backend.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Access {
    /// Sentinel for an unused log slot.
    None,
    Read {
        addr: usize,
        val: u32,
    },
    Write {
        addr: usize,
        val: u32,
    },
    Htvec {
        addr: usize,
        val: u8,
    },
}

/// In-memory recording backend: models the two MASK registers (so a
/// read-modify-write observes its own writes) and records every access for
/// later assertion. It never touches real MMIO.
struct RecorderPchPic {
    mask: [u32; PIC_REG_COUNT],
    htvec: [u8; PCH_PIC_IRQ_COUNT],
    accesses: [Access; 8],
    count: usize,
}

impl RecorderPchPic {
    const fn new() -> Self {
        Self {
            mask: [0; PIC_REG_COUNT],
            htvec: [0; PCH_PIC_IRQ_COUNT],
            accesses: [Access::None; 8],
            count: 0,
        }
    }

    fn record(&mut self, access: Access) {
        if self.count < self.accesses.len() {
            self.accesses[self.count] = access;
        }
        self.count += 1;
    }

    fn access_count(&self) -> usize {
        self.count
    }

    fn access(&self, i: usize) -> Access {
        self.accesses[i]
    }
}

impl PchPicRegs for RecorderPchPic {
    fn reg_read(&mut self, addr: usize) -> u32 {
        let idx = (addr - PCH_PIC_MASK) / core::mem::size_of::<u32>();
        let val = self.mask[idx.min(PIC_REG_COUNT - 1)];
        self.record(Access::Read { addr, val });
        val
    }

    fn reg_write(&mut self, addr: usize, val: u32) {
        let idx = (addr - PCH_PIC_MASK) / core::mem::size_of::<u32>();
        if idx < PIC_REG_COUNT {
            self.mask[idx] = val;
        }
        self.record(Access::Write { addr, val });
    }

    fn htvec_write(&mut self, addr: usize, val: u8) {
        let idx = addr - PCH_INT_HTVEC;
        if idx < PCH_PIC_IRQ_COUNT {
            self.htvec[idx] = val;
        }
        self.record(Access::Htvec { addr, val });
    }
}

#[def_test]
fn test_enable_legal_boundaries() {
    for irq in [0usize, 31, 32, 63] {
        let mut rec = RecorderPchPic::new();
        enable_irq_impl(&mut rec, irq);

        // read-modify-write of the MASK register, then one HTVEC byte.
        assert_eq!(rec.access_count(), 3);

        let expected_mask = PCH_PIC_MASK + (irq / PIC_COUNT_PER_REG) * 4;
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: expected_mask,
                val: 0
            }
        );
        assert_eq!(
            rec.access(1),
            Access::Write {
                addr: expected_mask,
                val: 0
            }
        );

        let expected_htvec = PCH_INT_HTVEC + irq;
        assert_eq!(
            rec.access(2),
            Access::Htvec {
                addr: expected_htvec,
                val: irq as u8
            }
        );
        assert_eq!(rec.htvec[irq], irq as u8);
    }
}

#[def_test]
fn test_enable_rejects_out_of_range() {
    for irq in [64usize, 3584, usize::MAX] {
        let mut rec = RecorderPchPic::new();
        enable_irq_impl(&mut rec, irq);
        // No register access before rejection: the log stays empty.
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_disable_legal_boundaries() {
    for irq in [0usize, 31, 32, 63] {
        let mut rec = RecorderPchPic::new();
        disable_irq_impl(&mut rec, irq);

        // disable does a MASK read-modify-write only; no HTVEC write.
        assert_eq!(rec.access_count(), 2);

        let expected_mask = PCH_PIC_MASK + (irq / PIC_COUNT_PER_REG) * 4;
        let bit = 1u32 << (irq % PIC_COUNT_PER_REG);
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: expected_mask,
                val: 0
            }
        );
        assert_eq!(
            rec.access(1),
            Access::Write {
                addr: expected_mask,
                val: bit
            }
        );
    }
}

#[def_test]
fn test_disable_rejects_out_of_range() {
    for irq in [64usize, 3584, usize::MAX] {
        let mut rec = RecorderPchPic::new();
        disable_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}
