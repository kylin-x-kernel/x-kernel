//! x86/x86_64 IRQ save/restore helpers.
use core::arch::asm;

/// Interrupt Enable Flag (IF)
const IF_BIT: usize = 1 << 9;

/// Save IF and disable interrupts.
#[inline]
pub fn save_disable() -> usize {
    let flags: usize;
    unsafe { asm!("pushf; pop {}; cli", out(reg) flags) };
    flags & IF_BIT
}

/// Restore IF according to saved flags.
#[inline]
pub fn restore(flags: usize) {
    if flags != 0 {
        unsafe { asm!("sti") };
    } else {
        unsafe { asm!("cli") };
    }
}
