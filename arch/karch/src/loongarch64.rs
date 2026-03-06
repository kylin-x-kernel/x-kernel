// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LoongArch64 low-level architecture operations.

use core::arch::asm;

use loongArch64::register::{crmd, ecfg, eentry, pgdh, pgdl};
use memaddr::{PhysAddr, VirtAddr};

/// Flushes the TLB.
///
/// If `vaddr` is [`None`], flushes the entire TLB. Otherwise, flushes the TLB
/// entry that maps the given virtual address.
#[inline]
pub fn flush_tlb(vaddr: Option<VirtAddr>) {
    unsafe {
        if let Some(vaddr) = vaddr {
            // <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#_dbar>
            //
            // Only after all previous load/store access operations are completely
            // executed, the DBAR 0 instruction can be executed; and only after the
            // execution of DBAR 0 is completed, all subsequent load/store access
            // operations can be executed.
            //
            // <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#_invtlb>
            //
            // formats: invtlb op, asid, addr
            //
            // op 0x5: Clear all page table entries with G=0 and ASID equal to the
            // register specified ASID, and VA equal to the register specified VA.
            //
            // When the operation indicated by op does not require an ASID, the
            // general register rj should be set to r0.
            asm!("dbar 0; invtlb 0x05, $r0, {reg}", reg = in(reg) vaddr.as_usize());
        } else {
            // op 0x0: Clear all page table entries
            asm!("dbar 0; invtlb 0x00, $r0, $r0");
        }
    }
}

/// Halt the current CPU.
#[inline]
pub fn stop_cpu() {
    disable_local_irq();
    unsafe { loongArch64::asm::idle() }
}

/// Relaxes the current CPU and waits for interrupts.
///
/// It must be called with interrupts enabled, otherwise it will never return.
#[inline]
pub fn await_interrupts() {
    unsafe { loongArch64::asm::idle() }
}

/// Allows the current CPU to respond to interrupts.
#[inline]
pub fn enable_local_irq() {
    crmd::set_ie(true)
}

/// Makes the current CPU ignore interrupts.
#[inline]
pub fn disable_local_irq() {
    crmd::set_ie(false)
}

/// Returns whether the current CPU is allowed to respond to interrupts.
#[inline]
pub fn local_irq_enabled() -> bool {
    crmd::read().ie()
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
/// Returns the saved CRMD value with the IE bit. Pass it to [`restore_irq`]
/// to restore the previous interrupt state.
#[inline]
pub fn save_irq_and_disable() -> usize {
    /// Interrupt Enable bit mask in CRMD.
    const IE_MASK: usize = 1 << 2;
    let mut flags: usize = 0;
    // csrxchg atomically reads CRMD and clears the IE bit
    unsafe { asm!("csrxchg {}, {}, 0x0", inout(reg) flags, in(reg) IE_MASK) };
    flags & IE_MASK
}

/// Restores local interrupt state from a value previously returned by
/// [`save_irq_and_disable`].
#[inline]
pub fn restore_irq(flags: usize) {
    /// Interrupt Enable bit mask in CRMD.
    const IE_MASK: usize = 1 << 2;
    // csrxchg atomically restores the IE bit
    unsafe { asm!("csrxchg {}, {}, 0x0", in(reg) flags, in(reg) IE_MASK) };
}

/// Reads the thread pointer of the current CPU (`$tp`).
///
/// It is used to implement TLS (Thread Local Storage).
#[inline]
pub fn read_thread_pointer() -> usize {
    let tp;
    unsafe { asm!("move {}, $tp", out(reg) tp) };
    tp
}

/// Writes the thread pointer of the current CPU (`$tp`).
///
/// It is used to implement TLS (Thread Local Storage).
///
/// # Safety
///
/// This function is unsafe as it changes the CPU states.
#[inline]
pub unsafe fn write_thread_pointer(val: usize) {
    unsafe { asm!("move $tp, {}", in(reg) val) }
}

/// Enables floating-point instructions by setting `EUEN.FPE`.
///
/// - `EUEN`: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#extended-component-unit-enable>
#[inline]
pub fn enable_fp() {
    loongArch64::register::euen::set_fpe(true);
}

/// Enables LSX extension by setting `EUEN.LSX`.
///
/// - `EUEN`: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#extended-component-unit-enable>
#[inline]
pub fn enable_lsx() {
    loongArch64::register::euen::set_sxe(true);
}

/// Reads the current page table root register for user space (`PGDL`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_user_page_table() -> PhysAddr {
    PhysAddr::from(pgdl::read().base())
}

/// Reads the current page table root register for kernel space (`PGDH`).
///
/// Returns the physical address of the page table root.
#[inline]
pub fn read_kernel_page_table() -> PhysAddr {
    PhysAddr::from(pgdh::read().base())
}

/// Writes the register to update the current page table root for user space
/// (`PGDL`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_user_page_table(root_paddr: PhysAddr) {
    pgdl::set_base(root_paddr.as_usize() as _);
}

/// Writes the register to update the current page table root for kernel space
/// (`PGDH`).
///
/// Note that the TLB is **NOT** flushed after this operation.
///
/// # Safety
///
/// This function is unsafe as it changes the virtual memory address space.
#[inline]
pub unsafe fn write_kernel_page_table(root_paddr: PhysAddr) {
    pgdh::set_base(root_paddr.as_usize());
}

/// Writes the Exception Entry Base Address register (`EENTRY`).
///
/// It also sets the Exception Configuration register (`ECFG`) to `VS=0`.
///
/// - ECFG: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#exception-configuration>
/// - EENTRY: <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#exception-entry-base-address>
///
/// # Safety
///
/// This function is unsafe as it changes the exception handling behavior of the
/// current CPU.
#[inline]
pub unsafe fn write_trap_vector_base(addr: usize) {
    ecfg::set_vs(0);
    eentry::set_eentry(addr);
}

/// Writes the Page Walk Controller registers (`PWCL` and `PWCH`).
///
/// The CSR numbers are inlined as numeric constants:
/// - `PWCL` = CSR 0x1c (lower-half page walk controller)
/// - `PWCH` = CSR 0x1d (higher-half page walk controller)
///
/// # Safety
///
/// This function is unsafe as it changes the page walk configuration such as
/// levels and starting bits.
///
/// - `PWCL` (CSR 0x1c): <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-lower-half-address-space>
/// - `PWCH` (CSR 0x1d): <https://loongson.github.io/LoongArch-Documentation/LoongArch-Vol1-EN.html#page-walk-controller-for-higher-half-address-space>
#[inline]
pub unsafe fn write_pwc(pwcl: u32, pwch: u32) {
    unsafe {
        asm!(
            "csrwr {}, 0x1c",
            "csrwr {}, 0x1d",
            in(reg) pwcl,
            in(reg) pwch
        )
    }
}
