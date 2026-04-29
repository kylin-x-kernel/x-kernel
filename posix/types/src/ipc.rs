// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX IPC types.

use linux_raw_sys::{
    ctypes::{c_long, c_ushort},
    general::{__kernel_gid_t, __kernel_key_t, __kernel_mode_t, __kernel_uid_t},
};

/// Data structure used to pass permission information to IPC operations.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern)]
pub struct IpcPerm {
    /// Key supplied to msgget(2)
    pub key: __kernel_key_t,
    /// Effective UID of owner
    pub uid: __kernel_uid_t,
    /// Effective GID of owner
    pub gid: __kernel_gid_t,
    /// Effective UID of creator
    pub cuid: __kernel_uid_t,
    /// Effective GID of creator
    pub cgid: __kernel_gid_t,
    /// Permissions (least significant 9 bits define access permissions)
    pub mode: __kernel_mode_t,
    /// Sequence number
    pub seq: c_ushort,
    /// Padding
    pub pad: c_ushort,
    /// Unused field
    pub unused0: c_long,
    /// Unused field
    pub unused1: c_long,
}
