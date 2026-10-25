// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

/// A recorded IOCSR access against the recording backend.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Access {
    /// Sentinel for an unused log slot.
    None,
    Read {
        addr: usize,
    },
    WriteD {
        addr: usize,
        val: u64,
    },
    WriteW {
        addr: usize,
        val: u32,
    },
}

/// In-memory recording backend with ENABLE/BOUNCE state. Other reads return
/// zero. It never touches real IOCSR.
struct RecorderEiointc<const N: usize> {
    accesses: [Access; N],
    count: usize,
    enable: [u64; VEC_REG_COUNT],
    bounce: [u64; VEC_REG_COUNT],
    has_unprotected_control_access: bool,
    has_masked_completion: bool,
}

impl<const N: usize> RecorderEiointc<N> {
    const fn new() -> Self {
        Self {
            accesses: [Access::None; N],
            count: 0,
            enable: [0; VEC_REG_COUNT],
            bounce: [0; VEC_REG_COUNT],
            has_unprotected_control_access: false,
            has_masked_completion: false,
        }
    }

    fn record(&mut self, access: Access) {
        let is_completion = matches!(access, Access::WriteD { addr, .. }
            if (EIOINTC_REG_ISR..EIOINTC_REG_ISR + VEC_REG_COUNT * 8).contains(&addr));
        if is_completion {
            self.has_masked_completion |= !karch::local_irq_enabled();
        } else {
            self.has_unprotected_control_access |= karch::local_irq_enabled();
            #[cfg(feature = "smp")]
            {
                self.has_unprotected_control_access |= REGISTER_LOCK.try_lock().is_some();
            }
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

impl<const N: usize> EiointcRegs for RecorderEiointc<N> {
    fn read_d(&mut self, addr: usize) -> u64 {
        self.record(Access::Read { addr });
        for (base, values) in [
            (EIOINTC_REG_ENABLE, &self.enable),
            (EIOINTC_REG_BOUNCE, &self.bounce),
        ] {
            if (base..base + VEC_REG_COUNT * 8).contains(&addr) {
                return values[(addr - base) / 8];
            }
        }
        0
    }

    fn write_d(&mut self, addr: usize, val: u64) {
        for (base, values) in [
            (EIOINTC_REG_ENABLE, &mut self.enable),
            (EIOINTC_REG_BOUNCE, &mut self.bounce),
        ] {
            if (base..base + VEC_REG_COUNT * 8).contains(&addr) {
                values[(addr - base) / 8] = val;
            }
        }
        self.record(Access::WriteD { addr, val });
    }

    fn write_w(&mut self, addr: usize, val: u32) {
        if (EIOINTC_REG_BOUNCE..EIOINTC_REG_BOUNCE + VEC_REG_COUNT * 8).contains(&addr) {
            let offset = addr - EIOINTC_REG_BOUNCE;
            let shift = (offset % 8) * 8;
            let word = &mut self.bounce[offset / 8];
            *word = (*word & !(u64::from(u32::MAX) << shift)) | (u64::from(val) << shift);
        }
        self.record(Access::WriteW { addr, val });
    }
}

#[def_test]
fn preparation_masks_all_vectors_before_routing() {
    // Preparation records 88 accesses; other operations need at most four.
    // Only this fixture reserves the larger log, with overflow checked below.
    let mut rec = RecorderEiointc::<128>::new();
    init_impl(&mut rec);
    assert!(rec.access_count() <= rec.accesses.len());
    for i in 0..VEC_REG_COUNT {
        assert_eq!(rec.access(i), Access::WriteD {
            addr: EIOINTC_REG_ENABLE + i * 8, val: 0,
        });
    }
    // No later routing write may reopen an enable bank.
    for access in &rec.accesses[VEC_REG_COUNT..rec.access_count()] {
        let addr = match access {
            Access::WriteD { addr, .. } | Access::WriteW { addr, .. } => *addr,
            _ => continue,
        };
        assert!(!(EIOINTC_REG_ENABLE..EIOINTC_REG_ENABLE + VEC_REG_COUNT * 8).contains(&addr));
    }
}

#[def_test]
fn test_enable_legal_boundaries() {
    for irq in [0usize, 63, 64, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        enable_irq_impl(&mut rec, irq);

        // ENABLE and BOUNCE, each as a read-modify-write.
        assert_eq!(rec.access_count(), 4);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        let bit = 1u64 << (irq % VEC_COUNT_PER_REG);
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: EIOINTC_REG_ENABLE + offset
            }
        );
        assert_eq!(
            rec.access(1),
            Access::WriteD {
                addr: EIOINTC_REG_ENABLE + offset,
                val: bit
            }
        );
        assert_eq!(
            rec.access(2),
            Access::Read {
                addr: EIOINTC_REG_BOUNCE + offset
            }
        );
        assert_eq!(
            rec.access(3),
            Access::WriteD {
                addr: EIOINTC_REG_BOUNCE + offset,
                val: bit
            }
        );
    }
}

#[def_test]
fn test_enable_rejects_out_of_range() {
    for irq in [256usize, 3584, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        enable_irq_impl(&mut rec, irq);
        // No IOCSR access before rejection: the log stays empty.
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_disable_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        disable_irq_impl(&mut rec, irq);

        // disable touches ENABLE only, as a read-modify-write.
        assert_eq!(rec.access_count(), 2);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        assert_eq!(
            rec.access(0),
            Access::Read {
                addr: EIOINTC_REG_ENABLE + offset
            }
        );
        assert_eq!(
            rec.access(1),
            Access::WriteD {
                addr: EIOINTC_REG_ENABLE + offset,
                val: 0
            }
        );
    }
}

#[def_test]
fn test_disable_rejects_out_of_range() {
    for irq in [256usize, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        disable_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_complete_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::<8>::new();
        complete_irq_impl(&mut rec, irq);

        // complete is a single ISR write.
        assert_eq!(rec.access_count(), 1);

        let offset = (irq / VEC_COUNT_PER_REG) * 8;
        let bit = 1u64 << (irq % VEC_COUNT_PER_REG);
        assert_eq!(
            rec.access(0),
            Access::WriteD {
                addr: EIOINTC_REG_ISR + offset,
                val: bit
            }
        );
    }
}

#[def_test]
fn test_complete_rejects_out_of_range() {
    for irq in [256usize, usize::MAX] {
        let mut rec = RecorderEiointc::<8>::new();
        complete_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn register_transactions_exclude_irq_and_preserve_other_vectors() {
    assert!(karch::local_irq_enabled());
    let mut preparation = RecorderEiointc::<128>::new();
    init_impl(&mut preparation);
    assert!(!preparation.has_unprotected_control_access);
    assert!(karch::local_irq_enabled());
    for bank in 0..VEC_REG_COUNT {
        let mut rec = RecorderEiointc::<16>::new();
        let first = bank * VEC_COUNT_PER_REG;
        let last = first + VEC_COUNT_PER_REG - 1;
        enable_irq_impl(&mut rec, first);
        enable_irq_impl(&mut rec, last);
        assert_eq!(rec.enable[bank], 1 | (1 << 63));
        assert_eq!(rec.bounce[bank], 1 | (1 << 63));
        disable_irq_impl(&mut rec, first);
        assert_eq!(rec.enable[bank], 1 << 63);
        assert_eq!(rec.bounce[bank], 1 | (1 << 63));
        disable_irq_impl(&mut rec, last);
        assert_eq!(rec.enable[bank], 0);
        assert!(!rec.has_unprotected_control_access);
        assert!(karch::local_irq_enabled());
    }
}

#[def_test]
fn completion_preserves_unmasked_context_and_writes_one_w1c() {
    // Only the recorder runs here, not live IOCSR. Starting unmasked makes
    // an accidental SpinNoIrq acquisition observable even in UP builds.
    assert!(karch::local_irq_enabled());
    let mut rec = RecorderEiointc::<8>::new();
    complete_irq_impl(&mut rec, 63);
    assert!(!rec.has_masked_completion);
    assert!(karch::local_irq_enabled());
    assert_eq!(rec.access_count(), 1);
    assert_eq!(rec.access(0), Access::WriteD { addr: EIOINTC_REG_ISR, val: 1 << 63 });
}
