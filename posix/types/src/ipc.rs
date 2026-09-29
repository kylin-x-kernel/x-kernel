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
    /// The 4-byte alignment hole Linux leaves after `__pad2` (`pad` here) on
    /// 64-bit targets; always zero so no uninitialized bytes reach user memory.
    pub abi_pad: u32,
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
    /// Queue ownership and access-mode carrier.
    pub msg_perm: IpcPerm,
    /// Last successful send time in Unix seconds.
    pub msg_stime: __kernel_time_t,
    /// Last successful receive time in Unix seconds.
    pub msg_rtime: __kernel_time_t,
    /// Last metadata change time in Unix seconds.
    pub msg_ctime: __kernel_time_t,
    /// Current queued payload bytes.
    pub msg_cbytes: __kernel_size_t,
    /// Number of queued messages.
    pub msg_qnum: __kernel_size_t,
    /// Maximum permitted queued payload bytes.
    pub msg_qbytes: __kernel_size_t,
    /// PID of the last sender.
    pub msg_lspid: __kernel_pid_t,
    /// PID of the last receiver.
    pub msg_lrpid: __kernel_pid_t,
}

/// A System V shared-memory descriptor.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, UserWrite, UserRead)]
pub struct shmid_ds {
    /// Shared-memory ownership and access-mode carrier.
    pub shm_perm: IpcPerm,
    /// Segment size in bytes.
    pub shm_segsz: __kernel_size_t,
    /// Last attach time in Unix seconds.
    pub shm_atime: __kernel_time_t,
    /// Last detach time in Unix seconds.
    pub shm_dtime: __kernel_time_t,
    /// Last metadata change time in Unix seconds.
    pub shm_ctime: __kernel_time_t,
    /// PID of the segment creator.
    pub shm_cpid: __kernel_pid_t,
    /// PID of the last attach/detach operation.
    pub shm_lpid: __kernel_pid_t,
    /// Attachment count in this ABI carrier.
    pub shm_nattch: c_ushort,
    /// Explicit trailing padding; initialize before copying to user space.
    pub abi_pad: [u8; 6],
}

/// A System V message payload header.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct msgbuf {
    /// Positive message type supplied before the payload.
    pub mtype: i64,
    /// Zero-length marker for the variable-length payload following the header.
    pub mtext: [u8; 0],
}

/// A Linux `msgctl(IPC_INFO)` result carrier.
#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct msginfo {
    /// Legacy message-pool size field, in KiB.
    pub msgpool: i32,
    /// Legacy maximum message-map entries field.
    pub msgmap: i32,
    /// Maximum size of one message payload in bytes.
    pub msgmax: i32,
    /// Default maximum queued payload bytes per queue.
    pub msgmnb: i32,
    /// Maximum queue identifiers.
    pub msgmni: i32,
    /// Legacy message-segment size in bytes.
    pub msgssz: i32,
    /// Legacy maximum message headers field.
    pub msgtql: i32,
    /// Legacy maximum message segments field.
    pub msgseg: u16,
    /// Explicit ABI padding; initialize before copying to user space.
    pub pad: u16,
}

// SAFETY: `msginfo` is a POD IPC info carrier whose bytes can be written back
// to user memory directly.
unsafe impl UserWrite for msginfo {}
