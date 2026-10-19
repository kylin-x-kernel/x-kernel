// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

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

/// In-memory recording backend: logs every access and returns zero for
/// reads, so read-modify-write results are fully predictable. It never
/// touches real IOCSR.
struct RecorderEiointc {
    accesses: [Access; 8],
    count: usize,
}

impl RecorderEiointc {
    const fn new() -> Self {
        Self {
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

impl EiointcRegs for RecorderEiointc {
    fn read_d(&mut self, addr: usize) -> u64 {
        self.record(Access::Read { addr });
        // The recorder does not model ENABLE/BOUNCE/ISR state; returning
        // zero makes `read | bit == bit` and `read & !bit == 0`.
        0
    }

    fn write_d(&mut self, addr: usize, val: u64) {
        self.record(Access::WriteD { addr, val });
    }

    fn write_w(&mut self, addr: usize, val: u32) {
        self.record(Access::WriteW { addr, val });
    }
}

#[def_test]
fn test_enable_legal_boundaries() {
    for irq in [0usize, 63, 64, 255] {
        let mut rec = RecorderEiointc::new();
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
        let mut rec = RecorderEiointc::new();
        enable_irq_impl(&mut rec, irq);
        // No IOCSR access before rejection: the log stays empty.
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_disable_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::new();
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
        let mut rec = RecorderEiointc::new();
        disable_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}

#[def_test]
fn test_complete_legal() {
    for irq in [0usize, 255] {
        let mut rec = RecorderEiointc::new();
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
        let mut rec = RecorderEiointc::new();
        complete_irq_impl(&mut rec, irq);
        assert_eq!(rec.access_count(), 0);
    }
}
