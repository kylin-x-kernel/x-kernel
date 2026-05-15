// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing synchronization structures.

use core::ffi::c_long;

use bytemuck::AnyBitPattern;

/// Maximum number of robust-list entries walked during exit cleanup.
pub const ROBUST_LIST_LIMIT: usize = linux_raw_sys::general::ROBUST_LIST_LIMIT as usize;

/// A robust futex list node.
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustList {
    /// Next list node.
    pub next: *mut RobustList,
}

/// The head of a robust futex list.
#[repr(C)]
#[derive(Debug, Copy, Clone, AnyBitPattern)]
pub struct RobustListHead {
    /// List head sentinel.
    pub list: RobustList,
    /// Offset from a list node to the futex word.
    pub futex_offset: c_long,
    /// Pending list operation entry.
    pub list_op_pending: *mut RobustList,
}
