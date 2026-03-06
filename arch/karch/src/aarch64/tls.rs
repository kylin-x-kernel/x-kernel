// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread-local storage operations for AArch64.

use aarch64_cpu::registers::{TPIDR_EL0, Readable, Writeable};

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
