// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V low-level architecture operations.

use memaddr::{PhysAddr, VirtAddr};
use riscv::{asm, register::{satp, sstatus, stvec}};

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    if let Some(vaddr) = vaddr {
        asm::sfence_vma(0, vaddr.as_usize())
    } else {
        asm::sfence_vma_all();
    }
}

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    riscv::asm::wfi(); // should never return
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    riscv::asm::wfi()
}

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    unsafe { sstatus::set_sie() }
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    unsafe { sstatus::clear_sie() }
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    sstatus::read().sie()
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
/// Returns the saved sstatus value with the SIE bit. Pass it to
/// [`restore_irq`] to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    /// Supervisor Interrupt Enable bit in sstatus.
    const SIE_BIT: usize = 1 << 1;
    let flags: usize;
    // csrrc: atomically clear the SIE bit and return the old sstatus value
    unsafe { core::arch::asm!("csrrc {}, sstatus, {}", out(reg) flags, const SIE_BIT) };
    flags & SIE_BIT
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    // csrrs: set the bits from `flags` back into sstatus
    unsafe { core::arch::asm!("csrrs x0, sstatus, {}", in(reg) flags) };
}

/// Reads the thread pointer of the current CPU (`tp`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    let tp;
    unsafe { core::arch::asm!("mv {}, tp", out(reg) tp) };
    tp
}

/// Writes the thread pointer of the current CPU (`tp`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(val: usize) {
    unsafe { core::arch::asm!("mv tp, {}", in(reg) val) }
}

/// Reads the current page table root register for user space (`satp`).
///
/// RISC-V does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_kernel_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    PhysAddr::from(satp::read().ppn() << 12)
}

/// Reads the current page table root register for kernel space (`satp`).
///
/// RISC-V does not have a separate page table root register for user and
/// kernel space, so this operation is the same as [`read_user_page_table`].
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    read_user_page_table()
}

/// Writes the register to update the current page table root for user space
/// (`satp`).
///
/// RISC-V does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_kernel_page_table`].
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    unsafe { satp::set(satp::Mode::Sv39, 0, root_paddr.as_usize() >> 12) };
}

/// Writes the register to update the current page table root for kernel space
/// (`satp`).
///
/// RISC-V does not have a separate page table root register for user
/// and kernel space, so this operation is the same as [`write_user_page_table`].
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    unsafe { write_user_page_table(root_paddr) };
}

/// Writes the Supervisor Trap Vector Base Address register (`stvec`).
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    let mut reg = stvec::read();
    reg.set_address(addr);
    reg.set_trap_mode(stvec::TrapMode::Direct);
    unsafe { stvec::write(reg) }
}
