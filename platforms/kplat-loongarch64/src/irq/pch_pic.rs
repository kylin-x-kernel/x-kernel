// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch LS7A PCH PIC (platform controller hub programmable interrupt
//! controller) driver.
//!
//! The PCH PIC owns [`PIC_REG_COUNT`] banks of [`PIC_COUNT_PER_REG`] lines,
//! i.e. [`PCH_PIC_IRQ_COUNT`] = 64 hardware IRQs. Every public entry point
//! rejects an out-of-range `hwirq` *before* any register access, so an
//! abnormal source number can neither compute an out-of-window register
//! offset (e.g. `PCH_INT_HTVEC + irq`) nor escape the mapped 4 KiB MMIO
//! aperture. Rejection is reported via `warn!` and never silently truncated,
//! taken modulo the line count, or remapped onto another legal IRQ.

use khal::mem::{PhysAddr, VirtAddr};
use lazyinit::LazyInit;

const PIC_COUNT_PER_REG: usize = 32;
const PIC_REG_COUNT: usize = 2;
/// Number of hardware IRQ lines owned by the PCH PIC: two 32-bit banks.
pub(crate) const PCH_PIC_IRQ_COUNT: usize = PIC_REG_COUNT * PIC_COUNT_PER_REG;
const PCH_PIC_MASK: usize = 0x20;
const PCH_PIC_EDGE: usize = 0x60;
const PCH_PIC_POL: usize = 0x3e0;
const PCH_INT_HTVEC: usize = 0x200;
const PCH_PIC_SIZE: usize = 0x1000;
const PCH_PIC_PADDR: usize = 0x1000_0000;

static PCH_PIC_BASE: LazyInit<VirtAddr> = LazyInit::new();

fn mmio_base() -> usize {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: PCH PIC initialization installs the MMIO \
                  mapping before interrupt operations are exposed"
    )]
    PCH_PIC_BASE
        .get()
        .expect("pch-pic iomap not initialized")
        .as_usize()
}

/// Register access surface for the PCH PIC.
///
/// The production implementation ([`MmioPchPic`]) talks to MMIO; tests
/// substitute a recording backend to assert that legal `hwirq` values touch
/// the expected offsets and that illegal values touch no register at all.
trait PchPicRegs {
    /// Read a 32-bit register at byte offset `addr` within the aperture.
    fn reg_read(&mut self, addr: usize) -> u32;
    /// Write a 32-bit register at byte offset `addr` within the aperture.
    fn reg_write(&mut self, addr: usize, val: u32);
    /// Write one byte to the HTVEC table at byte offset `addr`.
    fn htvec_write(&mut self, addr: usize, val: u8);
}

/// MMIO-backed register access; the production backend.
struct MmioPchPic;

impl PchPicRegs for MmioPchPic {
    fn reg_read(&mut self, addr: usize) -> u32 {
        // SAFETY: `mmio_base()` is the kernel virtual address of the 4 KiB
        // PCH PIC aperture established by `init()` via `memspace::iomap_device`.
        // Callers only pass offsets inside that aperture: `init()` uses the
        // fixed EDGE/POL offsets, and enable/disable reach here only after
        // `check_hwirq` proved `irq < PCH_PIC_IRQ_COUNT`, so the MASK offset
        // is in [0x20, 0x24] and the HTVEC offset in [0x200, 0x240). The
        // volatile read models device-memory semantics, not synchronization.
        unsafe { ((mmio_base() + addr) as *const u32).read_volatile() }
    }

    fn reg_write(&mut self, addr: usize, val: u32) {
        // SAFETY: same aperture and in-bounds argument as `reg_read`; the
        // volatile write is the externally observable device write and is not
        // used for synchronization.
        unsafe {
            ((mmio_base() + addr) as *mut u32).write_volatile(val);
        }
    }

    fn htvec_write(&mut self, addr: usize, val: u8) {
        // SAFETY: `addr` is `PCH_INT_HTVEC + irq` for an `irq` already proven
        // `< PCH_PIC_IRQ_COUNT` by `check_hwirq`, so it lands inside the
        // 64-byte HTVEC table within the mapped aperture. The byte write is
        // the hardware-defined entry format for this register block.
        unsafe {
            ((mmio_base() + addr) as *mut u8).write_volatile(val);
        }
    }
}

pub(super) fn init() {
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the PCH-PIC register aperture \
                  cannot be mapped; interrupt-controller initialization has no error return or \
                  recovery"
    )]
    let base = memspace::iomap_device(PhysAddr::from_usize(PCH_PIC_PADDR), PCH_PIC_SIZE, "pch-pic")
        .unwrap_or_else(|err| panic!("failed to iomap pch-pic: {err:?}"));
    PCH_PIC_BASE.init_once(base);
    let mut regs = MmioPchPic;
    for _ in 0..PIC_REG_COUNT {
        regs.reg_write(PCH_PIC_EDGE, 0);
        regs.reg_write(PCH_PIC_POL, 0);
    }
}

fn split_bit(irq: usize) -> (usize, u32) {
    (irq / PIC_COUNT_PER_REG * 4, 1 << (irq % PIC_COUNT_PER_REG))
}

/// Reject an out-of-range `hwirq` before any register access.
///
/// Returns `true` when `irq` is a valid PCH PIC line index. The check is
/// shared by the enable and disable paths so the failure path provably
/// performs no register read or write.
fn check_hwirq(irq: usize) -> bool {
    irq < PCH_PIC_IRQ_COUNT
}

fn enable_irq_impl<R: PchPicRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("pch-pic: reject out-of-range hwirq {irq} (valid 0..{PCH_PIC_IRQ_COUNT})");
        return;
    }
    let (offset, bit) = split_bit(irq);
    let addr = PCH_PIC_MASK + offset;
    let val = regs.reg_read(addr);
    regs.reg_write(addr, val & !bit);
    let addr = PCH_INT_HTVEC + irq;
    regs.htvec_write(addr, irq as u8);
}

fn disable_irq_impl<R: PchPicRegs>(regs: &mut R, irq: usize) {
    if !check_hwirq(irq) {
        warn!("pch-pic: reject out-of-range hwirq {irq} (valid 0..{PCH_PIC_IRQ_COUNT})");
        return;
    }
    let (offset, bit) = split_bit(irq);
    let addr = PCH_PIC_MASK + offset;
    let val = regs.reg_read(addr);
    regs.reg_write(addr, val | bit);
}

pub(super) fn enable_irq(irq: usize) {
    enable_irq_impl(&mut MmioPchPic, irq);
}

pub(super) fn disable_irq(irq: usize) {
    disable_irq_impl(&mut MmioPchPic, irq);
}

#[unittest::mod_test]
mod tests {
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
}
