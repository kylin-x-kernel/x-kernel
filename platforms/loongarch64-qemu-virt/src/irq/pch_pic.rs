// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use khal::mem::{PhysAddr, VirtAddr};
use lazyinit::LazyInit;

const PIC_COUNT_PER_REG: usize = 32;
const PIC_REG_COUNT: usize = 2;
const PCH_PIC_MASK: usize = 0x20;
const PCH_PIC_EDGE: usize = 0x60;
const PCH_PIC_POL: usize = 0x3e0;
const PCH_INT_HTVEC: usize = 0x200;
const PCH_PIC_SIZE: usize = 0x1000;
const PCH_PIC_PADDR: usize = 0x1000_0000;

static PCH_PIC_BASE: LazyInit<VirtAddr> = LazyInit::new();

fn mmio_base() -> usize {
    PCH_PIC_BASE
        .get()
        .expect("pch-pic iomap not initialized")
        .as_usize()
}

fn read_w(addr: usize) -> u32 {
    // SAFETY: `mmio_base()` is initialized from `iomap_device`, and callers
    // pass PCH PIC register offsets within the mapped 32-bit MMIO aperture.
    unsafe { ((mmio_base() + addr) as *mut u32).read_volatile() }
}
fn write_w(addr: usize, val: u32) {
    // SAFETY: `mmio_base()` is initialized from `iomap_device`, and callers
    // pass PCH PIC register offsets within the mapped 32-bit MMIO aperture.
    unsafe {
        ((mmio_base() + addr) as *mut u32).write_volatile(val);
    }
}
pub fn init() {
    let base = memspace::iomap_device(PhysAddr::from_usize(PCH_PIC_PADDR), PCH_PIC_SIZE, "pch-pic")
        .unwrap_or_else(|err| panic!("failed to iomap pch-pic: {err:?}"));
    PCH_PIC_BASE.init_once(base);
    for _ in 0..PIC_REG_COUNT {
        write_w(PCH_PIC_EDGE, 0);
        write_w(PCH_PIC_POL, 0);
    }
}
fn split_bit(irq: usize) -> (usize, u32) {
    (irq / PIC_COUNT_PER_REG * 4, 1 << (irq % PIC_COUNT_PER_REG))
}
pub fn enable_irq(irq: usize) {
    let (offset, bit) = split_bit(irq);
    let addr = PCH_PIC_MASK + offset;
    write_w(addr, read_w(addr) & !bit);
    let addr = PCH_INT_HTVEC + irq;
    // SAFETY: `addr` selects a byte entry inside the already-mapped HTVEC
    // table, and programming it with the IRQ number is the hardware-defined
    // format for this register block.
    unsafe {
        ((mmio_base() + addr) as *mut u8).write_volatile(irq as _);
    }
}
pub fn disable_irq(irq: usize) {
    let (offset, bit) = split_bit(irq);
    let addr = PCH_PIC_MASK + offset;
    write_w(addr, read_w(addr) | bit);
}
