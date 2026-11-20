// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `io_uring` ABI subset used by the async I/O implementation.
//!
//! Supported today: `io_uring_setup` with a single shared SQ/CQ mapping,
//! `io_uring_enter` submission and completion draining, and the NOP opcode.
//! Other Linux opcodes are consumed and completed with `-EINVAL` until the
//! execution phase implements them; unsupported Linux features (SQPOLL,
//! registered files/buffers, linked SQEs, `io_uring_register`, IOPOLL, ...)
//! are not advertised in `features`.

use core::mem::{offset_of, size_of};

use posix_types::{UserRead, UserWrite};

/// `io_uring_setup` mmap offset for the shared SQ/CQ ring.
pub const IORING_OFF_SQ_RING: usize = 0;
/// `io_uring_setup` mmap offset for the CQ ring (only with `SINGLE_MMAP` off).
pub const IORING_OFF_CQ_RING: usize = 0x0800_0000;
/// `io_uring_setup` mmap offset for the SQE array.
pub const IORING_OFF_SQES: usize = 0x1000_0000;

/// `io_uring_enter` flag: wait for at least `min_complete` completions.
pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;

/// Feature: SQ and CQ rings share one mmap.
pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;

/// `io_uring_setup` input flag bits supported by this subset.
pub const IORING_SETUP_FLAGS_SUPPORTED: u32 = 0;

/// `IORING_OP_NOP`.
pub const IORING_OP_NOP: u8 = 0;
/// `IORING_OP_READV`.
pub const IORING_OP_READV: u8 = 1;
/// `IORING_OP_WRITEV`.
pub const IORING_OP_WRITEV: u8 = 2;
/// `IORING_OP_FSYNC`.
pub const IORING_OP_FSYNC: u8 = 3;
/// `IORING_OP_READ`.
pub const IORING_OP_READ: u8 = 22;
/// `IORING_OP_WRITE`.
pub const IORING_OP_WRITE: u8 = 23;

/// Byte size of one Linux `struct io_uring_sqe`.
pub const IO_URING_SQE_SIZE: usize = 64;
/// Byte size of one Linux `struct io_uring_cqe`.
pub const IO_URING_CQE_SIZE: usize = 16;
/// Byte size of Linux `struct io_uring_params`.
pub const IO_URING_PARAMS_SIZE: usize = 120;

/// One Linux `struct io_uring_sqe` (64 bytes).
///
/// Field roles depend on `opcode`; the layout matches the Linux UAPI so a
/// subset implementation can consume liburing-built SQEs unchanged.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IoUringSqe {
    /// `IORING_OP_*`.
    pub opcode: u8,
    /// `IOSQE_*`.
    pub flags: u8,
    /// Request priority (unused by this subset).
    pub ioprio: u16,
    /// Target file descriptor (interpretation depends on `opcode`).
    pub fd: i32,
    /// Offset or second address operand.
    pub off: u64,
    /// User buffer address / first operand.
    pub addr: u64,
    /// Byte count.
    ///
    /// The field name is fixed by Linux `struct io_uring_sqe`; the unit is
    /// bytes.
    pub len: u32,
    /// Opcode-specific flags (rw_flags/fsync_flags/...).
    pub op_flags: u32,
    /// Opaque user cookie echoed in the CQE.
    pub user_data: u64,
    /// Buffer index / group (unused by this subset).
    pub buf_index: u16,
    /// Request personality (unused by this subset).
    pub personality: u16,
    /// splice fd / file index (unused by this subset).
    pub splice_fd_in: i32,
    /// Third address operand.
    pub addr3: u64,
    /// Tail padding.
    pub reserved: u64,
}

/// One Linux `struct io_uring_cqe` (16 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IoUringCqe {
    /// Cookie echoed from the originating SQE.
    pub user_data: u64,
    /// Non-negative result or negated errno.
    pub res: i32,
    /// Completion flags (zero in this subset).
    pub flags: u32,
}

/// Linux `struct io_sqring_offsets`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IoUringSqOffsets {
    /// Kernel-consumed submission index.
    pub head: u32,
    /// User-published submission index.
    pub tail: u32,
    /// `sq_entries - 1`.
    pub ring_mask: u32,
    /// Submission-ring depth.
    pub ring_entries: u32,
    /// Ring flags.
    pub flags: u32,
    /// Dropped submissions.
    pub dropped: u32,
    /// Offset of the u32 SQE-index array.
    pub array: u32,
    /// Reserved.
    pub resv1: u32,
    /// Registered user address (zero here).
    pub user_addr: u64,
}

/// Linux `struct io_cqring_offsets`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IoUringCqOffsets {
    /// User-consumed completion index.
    pub head: u32,
    /// Kernel-published completion index.
    pub tail: u32,
    /// `cq_entries - 1`.
    pub ring_mask: u32,
    /// Completion-ring depth.
    pub ring_entries: u32,
    /// Overflow count.
    pub overflow: u32,
    /// Offset of the CQE array.
    pub cqes: u32,
    /// Ring flags.
    pub flags: u32,
    /// Reserved.
    pub resv1: u32,
    /// Registered user address (zero here).
    pub user_addr: u64,
}

/// Linux `struct io_uring_params` (120 bytes, in/out).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IoUringParams {
    /// Requested SQ depth (out: granted depth).
    pub sq_entries: u32,
    /// Requested CQ depth (out: granted depth).
    pub cq_entries: u32,
    /// Setup flags.
    pub flags: u32,
    /// SQPOLL thread CPU (unsupported).
    pub sq_thread_cpu: u32,
    /// SQPOLL idle timeout (unsupported).
    pub sq_thread_idle: u32,
    /// Out: supported feature bits.
    pub features: u32,
    /// Out: worker queue fd (unsupported).
    pub wq_fd: u32,
    /// Reserved.
    pub resv: [u32; 3],
    /// Out: SQ ring field offsets.
    pub sq_off: IoUringSqOffsets,
    /// Out: CQ ring field offsets.
    pub cq_off: IoUringCqOffsets,
}

impl IoUringSqe {
    /// Returns whether this subset can execute the opcode.
    ///
    /// Only NOP executes in the phase-1 implementation. Other Linux opcodes
    /// are consumed and completed with `-EINVAL` instead of being reported as
    /// successful without doing the requested I/O.
    pub fn is_supported_opcode(&self) -> bool {
        matches!(self.opcode, IORING_OP_NOP)
    }
}

const _: () = {
    assert!(size_of::<IoUringSqe>() == IO_URING_SQE_SIZE);
    assert!(offset_of!(IoUringSqe, off) == 8);
    assert!(offset_of!(IoUringSqe, addr) == 16);
    assert!(offset_of!(IoUringSqe, len) == 24);
    assert!(offset_of!(IoUringSqe, user_data) == 32);
    assert!(offset_of!(IoUringSqe, addr3) == 48);
    assert!(size_of::<IoUringCqe>() == IO_URING_CQE_SIZE);
    assert!(offset_of!(IoUringCqe, res) == 8);
    assert!(offset_of!(IoUringCqe, flags) == 12);
    assert!(size_of::<IoUringParams>() == IO_URING_PARAMS_SIZE);
    assert!(offset_of!(IoUringParams, sq_off) == 40);
    assert!(offset_of!(IoUringParams, cq_off) == 80);
};
