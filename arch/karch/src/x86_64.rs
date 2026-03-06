// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 low-level architecture operations.

use core::arch::asm;

use memaddr::{MemoryAddr, PhysAddr, VirtAddr};
use x86::{controlregs, msr, tlb};
use x86_64::instructions::interrupts;

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    if let Some(vaddr) = vaddr {
        unsafe { tlb::flush(vaddr.into()) }
    } else {
        unsafe { tlb::flush_all() }
    }
}

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    await_interrupts(); // should never return
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    if cfg!(target_os = "none") {
        unsafe { asm!("hlt") }
    } else {
        core::hint::spin_loop()
    }
}

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    #[cfg(target_os = "none")]
    interrupts::enable()
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    #[cfg(target_os = "none")]
    interrupts::disable()
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    interrupts::are_enabled()
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
/// Returns the saved EFLAGS value with the IF bit. Pass it to [`restore_irq`]
/// to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    #[cfg(target_os = "none")]
    {
        /// Interrupt Enable Flag (IF).
        const IF_BIT: usize = 1 << 9;
        let flags: usize;
        unsafe { asm!("pushf; pop {}; cli", out(reg) flags) };
        flags & IF_BIT
    }
    #[cfg(not(target_os = "none"))]
    0
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    #[cfg(target_os = "none")]
    {
        if flags != 0 {
            unsafe { asm!("sti") };
        } else {
            unsafe { asm!("cli") };
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = flags;
}

/// Reads the thread pointer of the current CPU (`FS_BASE`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    unsafe { msr::rdmsr(msr::IA32_FS_BASE) as usize }
}

/// Writes the thread pointer of the current CPU (`FS_BASE`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(val: usize) {
    unsafe { msr::wrmsr(msr::IA32_FS_BASE, val as u64) }
}

/// Reads the current page table root register for user space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_kernel_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    PhysAddr::from(unsafe { controlregs::cr3() } as usize).align_down_4k()
}

/// Reads the current page table root register for kernel space (`CR3`).
///
/// x86_64 does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_user_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    read_user_page_table()
}

/// Writes the register to update the current page table root for user space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_kernel_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    unsafe { controlregs::cr3_write(root_paddr.as_usize() as _) }
}

/// Writes the register to update the current page table root for kernel space
/// (`CR3`).
///
/// x86_64 does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_user_page_table`].
///
/// Note that the TLB will be **flushed** after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    unsafe { write_user_page_table(root_paddr) }
}

/// Performs a hypercall to the hypervisor using the `vmmcall` instruction.
///
/// This is used on AMD/Hygon platforms for KVM hypercalls.
/// For Intel platforms, `vmcall` would be used instead.
///
/// # Arguments
/// * `nr` - Hypercall number (passed in RAX)
/// * `a0` - First argument (passed in RBX)
/// * `a1` - Second argument (passed in RCX)
///
/// # Returns
/// The return value from the hypervisor (from RAX).
#[inline]
pub fn hypercall(nr: u64, a0: u64, a1: u64) -> i64 {
    let ret: i64;
    unsafe {
        // Note: rbx is reserved by LLVM, so we need to save/restore it manually
        asm!(
            "push rbx",
            "mov rbx, {a0}",
            "vmmcall",
            "pop rbx",
            a0 = in(reg) a0,
            inout("rax") nr => ret,
            in("rcx") a1,
            options()
        );
    }
    ret
}
