// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

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
    has_unprotected_access: bool,
}

impl RecorderPchPic {
    const fn new() -> Self {
        Self {
            mask: [0; PIC_REG_COUNT],
            htvec: [0; PCH_PIC_IRQ_COUNT],
            accesses: [Access::None; 8],
            count: 0,
            has_unprotected_access: false,
        }
    }

    fn record(&mut self, access: Access) {
        self.has_unprotected_access |= karch::local_irq_enabled();
        // A deterministic contender at every register access must be excluded
        // by the same lock as production. UP has no spin flag to probe.
        #[cfg(feature = "smp")]
        {
            self.has_unprotected_access |= REGISTER_LOCK.try_lock().is_some();
        }
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
fn preparation_masks_both_banks_before_configuring_them() {
    let mut rec = RecorderPchPic::new();
    init_impl(&mut rec);
    assert_eq!(rec.mask, [u32::MAX; PIC_REG_COUNT]);
    assert_eq!(rec.access_count(), 6);
    for i in 0..PIC_REG_COUNT {
        assert_eq!(rec.access(i), Access::Write {
            addr: PCH_PIC_MASK + i * 4, val: u32::MAX,
        });
        assert_eq!(rec.access(2 + i * 2), Access::Write {
            addr: PCH_PIC_EDGE + i * 4, val: 0,
        });
        assert_eq!(rec.access(3 + i * 2), Access::Write {
            addr: PCH_PIC_POL + i * 4, val: 0,
        });
    }
}

#[def_test]
fn test_enable_legal_boundaries() {
    for irq in [0usize, 31, 32, 63] {
        let mut rec = RecorderPchPic::new();
        rec.mask = [u32::MAX; PIC_REG_COUNT];
        enable_irq_impl(&mut rec, irq);

        // The vector must be installed before the MASK read-modify-write.
        assert_eq!(rec.access_count(), 3);

        let expected_mask = PCH_PIC_MASK + (irq / PIC_COUNT_PER_REG) * 4;
        assert_eq!(
            rec.access(1),
            Access::Read {
                addr: expected_mask,
                val: u32::MAX
            }
        );
        assert_eq!(
            rec.access(2),
            Access::Write {
                addr: expected_mask,
                val: !(1 << (irq % PIC_COUNT_PER_REG))
            }
        );

        let expected_htvec = PCH_INT_HTVEC + irq;
        assert_eq!(
            rec.access(0),
            Access::Htvec {
                addr: expected_htvec,
                val: irq as u8
            }
        );
        assert_eq!(rec.htvec[irq], irq as u8);
        assert_eq!(rec.mask[1 - irq / PIC_COUNT_PER_REG], u32::MAX);
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

#[def_test]
fn register_transactions_exclude_irq_and_preserve_other_lines() {
    // Start unmasked so observing IRQ-off inside the backend has discrimination.
    assert!(karch::local_irq_enabled());
    let mut rec = RecorderPchPic::new();
    init_impl(&mut rec);
    assert!(karch::local_irq_enabled());
    assert!(!rec.has_unprotected_access);
    for bank in 0..PIC_REG_COUNT {
        let first = bank * PIC_COUNT_PER_REG;
        let last = first + PIC_COUNT_PER_REG - 1;
        enable_irq_impl(&mut rec, first);
        enable_irq_impl(&mut rec, last);
        assert_eq!(rec.mask[bank], !(1u32 | (1 << 31)));
        disable_irq_impl(&mut rec, first);
        assert_eq!(rec.mask[bank], !(1u32 << 31));
        disable_irq_impl(&mut rec, last);
        assert_eq!(rec.mask[bank], u32::MAX);
        assert!(!rec.has_unprotected_access);
        assert!(karch::local_irq_enabled());
    }
}
