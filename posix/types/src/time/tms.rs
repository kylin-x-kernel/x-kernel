// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing `times(2)` structures.

use crate::ptr::UserWrite;

#[repr(C)]
pub struct Tms {
    /// User time.
    pub tms_utime: usize,
    /// System time.
    pub tms_stime: usize,
    /// User time of children.
    pub tms_cutime: usize,
    /// System time of children.
    pub tms_cstime: usize,
}

unsafe impl UserWrite for Tms {}
