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
    edge: [u32; PIC_REG_COUNT],
    polarity: [u32; PIC_REG_COUNT],
    rejects_edge: bool,
    htvec: [u8; PCH_PIC_IRQ_COUNT],
    accesses: [Access; 8],
    count: usize,
    has_unprotected_access: bool,
}

impl RecorderPchPic {
    const fn new() -> Self {
        Self {
            mask: [0; PIC_REG_COUNT],
            edge: [0; PIC_REG_COUNT],
            polarity: [0; PIC_REG_COUNT],
            rejects_edge: false,
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
        let val = match addr {
            PCH_PIC_MASK..=0x24 => self.mask[(addr - PCH_PIC_MASK) / 4],
            PCH_PIC_EDGE..=0x64 => self.edge[(addr - PCH_PIC_EDGE) / 4],
            PCH_PIC_POL..=0x3e4 => self.polarity[(addr - PCH_PIC_POL) / 4],
            _ => panic!("unexpected register read"),
        };
        self.record(Access::Read { addr, val });
        val
    }

    fn reg_write(&mut self, addr: usize, val: u32) {
        let idx = (addr - PCH_PIC_MASK) / core::mem::size_of::<u32>();
        if idx < PIC_REG_COUNT {
            self.mask[idx] = val;
        }
        if (PCH_PIC_EDGE..=0x64).contains(&addr) && !self.rejects_edge {
            self.edge[(addr - PCH_PIC_EDGE) / 4] = val;
        }
        if (PCH_PIC_POL..=0x3e4).contains(&addr) {
            self.polarity[(addr - PCH_PIC_POL) / 4] = val;
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

#[def_test]
fn checked_level_configuration_preserves_other_bits_and_restores_mask() {
    for irq in [0, 31, 32, 63] {
        for was_masked in [false, true] {
            let mut rec = RecorderPchPic::new();
            rec.edge = [u32::MAX; 2];
            rec.polarity = [u32::MAX; 2];
            let bank = irq / 32;
            let bit = 1 << (irq % 32);
            rec.mask[bank] = if was_masked { bit } else { 0 };
            let old_mask = rec.mask;
            assert!(configure_level_impl(&mut rec, irq, false));
            assert_eq!(rec.mask, old_mask);
            assert_eq!(rec.edge[bank], !bit);
            assert_eq!(rec.polarity[bank], !bit);
            assert_eq!(rec.edge[1 - bank], u32::MAX);
            assert_eq!(rec.polarity[1 - bank], u32::MAX);
            assert!(!rec.has_unprotected_access);
            assert!(configure_level_impl(&mut rec, irq, true));
            assert_eq!(rec.polarity[bank], u32::MAX);
        }
    }
}

#[def_test]
fn failed_level_readback_keeps_source_masked_and_invalid_pin_has_no_access() {
    let mut rec = RecorderPchPic::new();
    rec.edge = [u32::MAX; 2];
    rec.rejects_edge = true;
    assert!(!configure_level_impl(&mut rec, 37, false));
    assert_eq!(rec.mask, [0, 1 << 5]);
    for irq in [64, usize::MAX] {
        let count = rec.access_count();
        assert!(!configure_level_impl(&mut rec, irq, false));
        clear_cause_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), count);
    }
}

#[def_test]
fn leaf_clear_is_one_w1c_write_not_a_parent_ack_or_read_modify_write() {
    for irq in [0, 31, 32, 63] {
        let mut rec = RecorderPchPic::new();
        clear_cause_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 1);
        assert_eq!(rec.access(0), Access::Write {
            addr: PCH_PIC_CLR + (irq / 32) * 4, val: 1 << (irq % 32),
        });
    }
}
