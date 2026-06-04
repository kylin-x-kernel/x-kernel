// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helper functions to initialize the CPU states on systems bootstrapping.

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the trap vector on RISC-V platforms.
pub fn init_trap() {
    crate::userspace_common::init_exception_table();
    unsafe extern "C" {
        fn trap_vector_base();
    }
    // SAFETY: Setting SUM bit allows supervisor to access user pages; required for `copy_user`. `trap_vector_base` is defined in `excp.S` with 4-byte alignment.
    unsafe {
        riscv::register::sstatus::set_sum();
        karch::write_trap_vector_base(trap_vector_base as *const () as usize);
    }
}
