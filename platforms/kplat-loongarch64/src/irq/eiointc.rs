// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch EIOINTC (extended I/O interrupt controller) driver.
//!
//! The EIOINTC owns [`VEC_COUNT`] = 256 interrupt vectors accessed via IOCSR.
//! Every public mutator rejects an out-of-range `hwirq` *before* any IOCSR
//! access, so an abnormal source number can neither compute an out-of-window
//! IOCSR offset nor corrupt an unrelated vector's enable/bounce/ISR state.
//! Rejection is reported via `warn!` and never silently truncated, taken
//! modulo the vector count, or remapped onto another legal vector.

use loongArch64::iocsr::{iocsr_read_d, iocsr_write_d, iocsr_write_w};

const LOONGARCH_IOCSR_MISC_FUNC: usize = 0x420;
const IOCSR_MISC_FUNC_EXT_IOI_EN: u64 = 1 << 48;
const EIOINTC_REG_NODEMAP: usize = 0x14a0;
const EIOINTC_REG_IPMAP: usize = 0x14c0;
const EIOINTC_REG_ENABLE: usize = 0x1600;
const EIOINTC_REG_BOUNCE: usize = 0x1680;
const EIOINTC_REG_ISR: usize = 0x1800;
const EIOINTC_REG_ROUTE: usize = 0x1c00;
const VEC_REG_COUNT: usize = 4;
const VEC_COUNT_PER_REG: usize = 64;
const VEC_COUNT: usize = VEC_REG_COUNT * VEC_COUNT_PER_REG;

/// Register access surface for the EIOINTC.
///
/// The production implementation ([`IocsrEiointc`]) talks to IOCSR; tests
/// substitute a recording backend to assert that legal `hwirq` values touch
/// the expected offsets and that illegal values touch no register at all.
trait EiointcRegs {
    /// Read a 64-bit IOCSR register at byte offset `addr`.
    fn read_d(&mut self, addr: usize) -> u64;
    /// Write a 64-bit IOCSR register at byte offset `addr`.
    fn write_d(&mut self, addr: usize, val: u64);
    /// Write a 32-bit IOCSR register at byte offset `addr`.
    fn write_w(&mut self, addr: usize, val: u32);
}

/// IOCSR-backed register access; the production backend.
struct IocsrEiointc;

impl EiointcRegs for IocsrEiointc {
    fn read_d(&mut self, addr: usize) -> u64 {
        iocsr_read_d(addr)
    }

    fn write_d(&mut self, addr: usize, val: u64) {
        iocsr_write_d(addr, val);
    }

    fn write_w(&mut self, addr: usize, val: u32) {
        iocsr_write_w(addr, val);
    }
}

pub(super) fn init() {
    let mut regs = IocsrEiointc;
    let misc = regs.read_d(LOONGARCH_IOCSR_MISC_FUNC);
    regs.write_d(LOONGARCH_IOCSR_MISC_FUNC, misc | IOCSR_MISC_FUNC_EXT_IOI_EN);
    let index = 0;
    for i in 0..(VEC_COUNT / 32) {
        let data = ((1 << (i * 2 + 1)) << 16) | (1 << (i * 2));
        regs.write_w(EIOINTC_REG_NODEMAP + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 32 / 4) {
        let bit = 1 << (1 + index);
        let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
        regs.write_w(EIOINTC_REG_IPMAP + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 4) {
        let bit = 1;
        let data = bit | (bit << 8) | (bit << 16) | (bit << 24);
        regs.write_w(EIOINTC_REG_ROUTE + i * 4, data);
    }
    for i in 0..(VEC_COUNT / 32) {
        regs.write_w(EIOINTC_REG_BOUNCE + i * 4, u32::MAX);
    }
}

fn split_bit(irq: usize) -> (usize, u64) {
    (irq / VEC_COUNT_PER_REG * 8, 1 << (irq % VEC_COUNT_PER_REG))
}

/// Reject an out-of-range `hwirq` before any IOCSR access.
///
/// Returns `true` when `irq` is a valid EIOINTC vector index. Shared by the
/// enable, disable and complete paths so the failure path provably performs
/// no register read or write.
fn check_hwirq(irq: usize) -> bool {
    irq < VEC_COUNT
}

fn enable_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    let (offset, bit) = split_bit(irq);
    for base in [EIOINTC_REG_ENABLE, EIOINTC_REG_BOUNCE] {
        let addr = base + offset;
        let val = regs.read_d(addr);
        regs.write_d(addr, val | bit);
    }
}

fn disable_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    let (offset, bit) = split_bit(irq);
    let addr = EIOINTC_REG_ENABLE + offset;
    let val = regs.read_d(addr);
    regs.write_d(addr, val & !bit);
}

fn complete_irq_impl<R: EiointcRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("eiointc: reject out-of-range hwirq {irq} (valid 0..{VEC_COUNT})");
        return;
    }
    let (offset, bit) = split_bit(irq);
    regs.write_d(EIOINTC_REG_ISR + offset, bit);
}

pub(super) fn enable_irq(irq: usize) {
    enable_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn disable_irq(irq: usize) {
    disable_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn complete_irq(irq: usize) {
    complete_irq_impl(&mut IocsrEiointc, irq);
}

pub(super) fn claim_irq() -> Option<usize> {
    for i in 0..(VEC_COUNT / 64) {
        let flags = iocsr_read_d(EIOINTC_REG_ISR + i * 8);
        if flags != 0 {
            return Some(flags.trailing_zeros() as usize + 64 * i);
        }
    }
    None
}

#[unittest::mod_test]
mod tests {
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
}
