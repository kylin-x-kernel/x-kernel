// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 low-level architecture operations.

use core::arch::asm;

use aarch64_cpu::{asm::barrier, registers::*};
use memaddr::{PhysAddr, VirtAddr};

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    if let Some(vaddr) = vaddr {
        const VA_MASK: usize = (1 << 44) - 1; // VA[55:12] => bits[43:0]
        let operand = (vaddr.as_usize() >> 12) & VA_MASK;

        #[cfg(not(feature = "arm-el2"))]
        unsafe {
            // TLB Invalidate by VA, All ASID, EL1, Inner Shareable
            asm!("tlbi vaae1is, {}; dsb sy; isb", in(reg) operand)
        }
        #[cfg(feature = "arm-el2")]
        unsafe {
            // TLB Invalidate by VA, EL2, Inner Shareable
            asm!("tlbi vae2is, {}; dsb sy; isb", in(reg) operand)
        }
    } else {
        // flush the entire TLB
        #[cfg(not(feature = "arm-el2"))]
        unsafe {
            // TLB Invalidate by VMID, All at stage 1, EL1
            asm!("dsb sy; isb; tlbi vmalle1; dsb sy; isb")
        }
        #[cfg(feature = "arm-el2")]
        unsafe {
            // TLB Invalidate All, EL2
            asm!("tlbi alle2; dsb sy; isb")
        }
    }
}

/// Flushes the entire instruction cache.
#[inline]
pub fn flush_icache_all() {
    unsafe { asm!("ic iallu; dsb sy; isb") };
}

/// Flushes the data cache line at the given virtual address.
///
/// Uses the `DC IVAC` instruction (Data Cache Invalidate by Virtual Address to
/// Point of Coherency). The cache line size is implementation-defined; 64 bytes
/// is typical for AArch64 but may vary across CPU implementations.
#[inline]
pub fn flush_dcache_line(vaddr: VirtAddr) {
    unsafe { asm!("dc ivac, {0:x}; dsb sy; isb", in(reg) vaddr.as_usize()) };
}

/// Halt the current CPU.
///
/// Disables interrupts then executes WFI. Since interrupts are disabled,
/// this should stop execution until reset.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    aarch64_cpu::asm::wfi();
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    aarch64_cpu::asm::wfi();
}

/// Allows the current CPU to respond to interrupts (clears DAIF.I).
#[inline]
pub fn enable_local_irq() {
    DAIF.write(DAIF::I::Unmasked);
}

/// Makes the current CPU ignore interrupts (sets DAIF.I).
#[inline]
pub fn disable_local_irq() {
    DAIF.write(DAIF::I::Masked);
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    !DAIF.is_set(DAIF::I)
}

/// Deprecated: use [`enable_local_irq`] instead.
#[deprecated(note = "Use `enable_local_irq` instead")]
#[inline]
pub fn enable_irq() {
    enable_local_irq()
}

/// Deprecated: use [`disable_local_irq`] instead.
#[deprecated(note = "Use `disable_local_irq` instead")]
#[inline]
pub fn disable_irq() {
    disable_local_irq()
}

/// Deprecated: use [`local_irq_enabled`] instead.
#[deprecated(note = "Use `local_irq_enabled` instead")]
#[inline]
pub fn irq_enabled() -> bool {
    local_irq_enabled()
}

/// Saves the current local interrupt state and disables interrupts atomically.
///
/// Returns the saved DAIF register value. Pass it to [`restore_irq`] to
/// restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    let flags: usize;
    unsafe {
        asm!("mrs {}, daif", out(reg) flags, options(nomem, nostack, preserves_flags));
        asm!("msr daifset, #2", options(nomem, nostack));
    }
    flags
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    unsafe {
        asm!("msr daif, {}", in(reg) flags, options(nomem, nostack));
    }
}

/// Reads the thread pointer of the current CPU (`TPIDR_EL0`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    TPIDR_EL0.get() as usize
}

/// Writes the thread pointer of the current CPU (`TPIDR_EL0`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the current CPU states.
#[inline]
pub unsafe fn write_thread_pointer(val: usize) {
    TPIDR_EL0.set(val as _)
}

/// Enable FP/SIMD instructions by setting the `FPEN` field in `CPACR_EL1`.
#[inline]
pub fn enable_fp() {
    CPACR_EL1.write(CPACR_EL1::FPEN::TrapNothing);
    barrier::isb(barrier::SY);
}

/// Reads the current page table root register for kernel space.
///
/// When the `arm-el2` feature is enabled, reads `TTBR0_EL2`; otherwise
/// reads `TTBR1_EL1`.
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    let pt_root_reg: usize;

    #[cfg(not(feature = "arm-el2"))]
    {
        pt_root_reg = TTBR1_EL1.get() as usize;
    }

    #[cfg(feature = "arm-el2")]
    {
        pt_root_reg = TTBR0_EL2.get() as usize;
    }

    PhysAddr::from(pt_root_reg)
}

/// Reads the current page table root register for user space (`TTBR0_EL1`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    let val = TTBR0_EL1.get();
    PhysAddr::from(val as usize)
}

/// Writes the register to update the current page table root for kernel space.
///
/// When the `arm-el2` feature is enabled, writes `TTBR0_EL2`; otherwise
/// writes `TTBR1_EL1`.
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    #[cfg(not(feature = "arm-el2"))]
    {
        TTBR1_EL1.set(root_paddr.as_usize() as _);
    }

    #[cfg(feature = "arm-el2")]
    {
        TTBR0_EL2.set(root_paddr.as_usize() as _);
    }
}

/// Writes the register to update the current page table root for user space
/// (`TTBR0_EL1`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    TTBR0_EL1.set(root_paddr.as_usize() as _);
}

/// Writes the exception vector base address register (`VBAR_EL1` or `VBAR_EL2`).
///
/// When the `arm-el2` feature is enabled, writes `VBAR_EL2`; otherwise
/// writes `VBAR_EL1`.
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    #[cfg(not(feature = "arm-el2"))]
    VBAR_EL1.set(addr as _);
    #[cfg(feature = "arm-el2")]
    VBAR_EL2.set(addr as _);
}
