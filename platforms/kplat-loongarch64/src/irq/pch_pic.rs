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
        // Callers only pass offsets inside that aperture: `init_impl()` writes
        // both banks at MASK 0x20/0x24, EDGE 0x60/0x64 and POL 0x3e0/0x3e4.
        // Enable/disable reach here only after
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
    init_impl(&mut MmioPchPic);
}

fn init_impl<R: PchPicRegs>(regs: &mut R) {
    // Mask both banks before programming trigger/polarity, as in Linux's
    // pch_pic_reset. Activation must not inherit firmware's unmasked lines.
    for i in 0..PIC_REG_COUNT {
        regs.reg_write(PCH_PIC_MASK + i * 4, u32::MAX);
    }
    for i in 0..PIC_REG_COUNT {
        regs.reg_write(PCH_PIC_EDGE + i * 4, 0);
        regs.reg_write(PCH_PIC_POL + i * 4, 0);
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
    // Publish the vector before opening the source gate.
    regs.htvec_write(PCH_INT_HTVEC + irq, irq as u8);
    let addr = PCH_PIC_MASK + offset;
    let val = regs.reg_read(addr);
    regs.reg_write(addr, val & !bit);
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

unittest::path_mod_test!(tests, "../../ut-tests/irq_pch_pic.rs");
