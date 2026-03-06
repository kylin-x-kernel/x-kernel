// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrapper functions for assembly instructions.

pub use karch::{
    await_interrupts, flush_tlb, read_thread_pointer, stop_cpu, write_thread_pointer,
};
// Re-exported with legacy names for backward compatibility.
pub use karch::{disable_local_irq as disable_local, enable_local_irq as enable_local, local_irq_enabled as is_enabled};

pub use karch::{
    read_kernel_page_table, read_user_page_table, restore_irq, save_irq_and_disable,
    write_kernel_page_table, write_trap_vector_base, write_user_page_table,
};

#[cfg(feature = "uspace")]
core::arch::global_asm!(include_asm_macros!(), include_str!("copy_user.S"));

#[cfg(feature = "uspace")]
unsafe extern "C" {
    /// Copies data from source to destination, where addresses may be in user
    /// space. Equivalent to memcpy.
    ///
    /// # Safety
    /// This function is unsafe because it performs raw memory operations.
    ///
    /// # Returns
    /// Returns the number of bytes not copied. This means 0 indicates success,
    /// while a value > 0 indicates failure.
    pub fn user_copy(dst: *mut u8, src: *const u8, size: usize) -> usize;
}

