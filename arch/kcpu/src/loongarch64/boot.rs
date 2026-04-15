// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helper functions to initialize LoongArch64 CPU bootstrap state.

/// Initializes trap handling on the current CPU.
///
/// In detail, it initializes the exception vector on LoongArch64 platforms.
pub fn init_trap() {
    crate::userspace_common::init_exception_table();
    unsafe {
        unsafe extern "C" {
            fn exception_entry_base();
        }
        karch::init_trap_state(exception_entry_base as *const () as usize);
    }
}
