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
    // SAFETY: Boot runs in supervisor mode; setting SUM enables the user-copy access policy on this
    // hart.
    unsafe { riscv::register::sstatus::set_sum() };
    // SAFETY: excp.S supplies the live, four-byte-aligned vector before this hart enables traps.
    unsafe { karch::write_trap_vector_base(trap_vector_base as *const () as usize) };
}
