// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX IPC types.

use linux_raw_sys::{
    ctypes::{c_long, c_ushort},
    general::{
        __kernel_gid_t, __kernel_key_t, __kernel_mode_t, __kernel_pid_t, __kernel_size_t,
        __kernel_time_t, __kernel_uid_t,
    },
};

use crate::{UserRead, UserWrite};

/// Data structure used to pass permission information to IPC operations.
#[repr(C)]
#[derive(Clone, Copy, UserWrite)]
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

/// A System V message queue descriptor.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, UserWrite, UserRead)]
pub struct msqid_ds {
    pub msg_perm: IpcPerm,
    pub msg_stime: __kernel_time_t,
    pub msg_rtime: __kernel_time_t,
    pub msg_ctime: __kernel_time_t,
    pub msg_cbytes: __kernel_size_t,
    pub msg_qnum: __kernel_size_t,
    pub msg_qbytes: __kernel_size_t,
    pub msg_lspid: __kernel_pid_t,
    pub msg_lrpid: __kernel_pid_t,
}

/// A System V shared-memory descriptor.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, UserWrite, UserRead)]
pub struct shmid_ds {
    pub shm_perm: IpcPerm,
    pub shm_segsz: __kernel_size_t,
    pub shm_atime: __kernel_time_t,
    pub shm_dtime: __kernel_time_t,
    pub shm_ctime: __kernel_time_t,
    pub shm_cpid: __kernel_pid_t,
    pub shm_lpid: __kernel_pid_t,
    pub shm_nattch: c_ushort,
    pub abi_pad: [u8; 6],
}

/// A System V message payload header.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct msgbuf {
    pub mtype: i64,
    pub mtext: [u8; 0],
}

/// A Linux `msgctl(IPC_INFO)` result carrier.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct msginfo {
    pub msgpool: i32,
    pub msgmap: i32,
    pub msgmax: i32,
    pub msgmnb: i32,
    pub msgmni: i32,
    pub msgssz: i32,
    pub msgtql: i32,
    pub msgseg: u16,
    pub pad: u16,
}

unsafe impl UserWrite for msginfo {}
