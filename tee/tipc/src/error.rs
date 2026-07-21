// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel TIPC syscall error conversion.

use kerrno::{KError, KErrorKind};
use linux_sysno::Sysno;
use log::warn;

// TIPC error codes returned to userspace via syscalls.
//
// Source-of-truth mirror: keep these in sync with
// rust-libtipc::tipc_types_sys::err::ERR_* in
// The kernel keeps a local copy to avoid depending on a userspace crate.
const ERR_GENERIC: i32 = -1;
const ERR_NOT_ENOUGH_BUFFER: i32 = -2;
const ERR_INVALID_ARGS: i32 = -4;
const ERR_NOT_FOUND: i32 = -5;
const ERR_ALREADY_EXISTS: i32 = -7;
const ERR_NO_MEMORY: i32 = -8;
const ERR_NO_RESOURCES: i32 = -9;
const ERR_BUSY: i32 = -10;
const ERR_NOT_READY: i32 = -11;
const ERR_NO_MSG: i32 = -12;
const ERR_TIMED_OUT: i32 = -18;
const ERR_CHANNEL_CLOSED: i32 = -19;
const ERR_NOT_ALLOWED: i32 = -21;
const ERR_NOT_SUPPORTED: i32 = -28;
const ERR_TOO_BIG: i32 = -29;
const ERR_BAD_HANDLE: i32 = -39;
const ERR_NOT_CONNECTED: i32 = -40;
const ERR_ACCESS_DENIED: i32 = -41;

/// Maps a kernel KError to a negative TIPC error code.
pub fn kerror_to_tipc_errno(err: KError, sysno: Sysno) -> i32 {
    use kerrno::KErrorKind::*;

    let kind = KErrorKind::try_from(err).unwrap_or_else(|linux_err| {
        warn!("tipc errno fallback for LinuxError: {linux_err:?}");
        Unsupported
    });
    match kind {
        WouldBlock => match sysno {
            Sysno::tipc_send_msg => ERR_NOT_ENOUGH_BUFFER,
            Sysno::tipc_get_msg => ERR_NO_MSG,
            _ => ERR_NOT_READY,
        },
        NotConnected => ERR_NOT_CONNECTED,
        BrokenPipe | ConnectionRefused | ConnectionReset | ConnectionAborted => ERR_CHANNEL_CLOSED,
        NotFound => ERR_NOT_FOUND,
        NoMemory => ERR_NO_MEMORY,
        StorageFull => ERR_NO_RESOURCES,
        InvalidInput | InvalidData | BadAddress | BadState => ERR_INVALID_ARGS,
        Unsupported => ERR_NOT_SUPPORTED,
        TimedOut => ERR_TIMED_OUT,
        PermissionDenied => ERR_ACCESS_DENIED,
        OperationNotPermitted => ERR_NOT_ALLOWED,
        ResourceBusy => ERR_BUSY,
        BadFileDescriptor => ERR_BAD_HANDLE,
        AlreadyExists => ERR_ALREADY_EXISTS,
        Interrupted | InProgress => ERR_NOT_READY,
        OutOfRange | FileTooLarge => ERR_TOO_BIG,
        _ => ERR_GENERIC,
    }
}
