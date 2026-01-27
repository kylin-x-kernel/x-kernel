use core::arch::asm;

const IE_MASK: usize = 1 << 2;

#[inline]
pub fn save_disable() -> usize {
    let mut flags: usize = 0;
    // clear the `IE` bit, and return the old CSR
    unsafe { asm!("csrxchg {}, {}, 0x0", inout(reg) flags, in(reg) IE_MASK) };
    flags & IE_MASK
}

#[inline]
pub fn restore(flags: usize) {
    // restore the `IE` bit
    unsafe { asm!("csrxchg {}, {}, 0x0", in(reg) flags, in(reg) IE_MASK) };
}
