// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `io_uring_setup` / `io_uring_enter` subset.
//!
//! Supported: a single shared SQ/CQ mmap, SQE submission, CQ draining, and
//! the `GETEVENTS` wait mode. SQPOLL, registration, linked operations and
//! registered files/buffers are not advertised or implemented.

use kasyncio::{
    AsyncioFile,
    abi::{IORING_ENTER_GETEVENTS, IORING_SETUP_FLAGS_SUPPORTED, IoUringParams},
    setup,
};
use kerrno::{KError, KResult};
use posix_types::{UserConstPtr, UserPtr, k_sigset};

/// Creates an io_uring context and returns its file descriptor.
///
/// `entries` is rounded up by the caller to a power of two. The out-fields of
/// `params` are filled with the granted depths, feature bits, and ring offsets
/// before the descriptor is installed.
///
/// # Execution context
///
/// Requires a current process user thread: it charges the caller's credentials
/// and installs the descriptor in the caller's fd table. It may sleep while
/// allocating ring pages and copying the user parameter block, so it must not
/// run in interrupt context or while holding a spinlock or an IRQ/preemption
/// disabled guard.
pub(crate) fn sys_io_uring_setup(entries: u32, params: UserPtr<IoUringParams>) -> KResult<isize> {
    let params = params.check_non_null().ok_or(KError::BadAddress)?;
    let mut value = params.read_vm()?;
    if value.flags & !IORING_SETUP_FLAGS_SUPPORTED != 0 {
        return Err(KError::InvalidInput);
    }
    // Linux rejects nonzero reserved words before allocating anything.
    if value.resv != [0; 3] {
        return Err(KError::InvalidInput);
    }
    // Fail an unwritable parameter block before allocating ring pages.
    params.write_vm(value)?;

    let created = setup(entries, kprocess::current_cred())?;
    value.sq_entries = created.sq_entries;
    value.cq_entries = created.cq_entries;
    value.features = created.features;
    value.wq_fd = 0;
    value.sq_off = created.sq_off;
    value.cq_off = created.cq_off;

    // Linux installs the io_uring fd with O_CLOEXEC unconditionally.
    let fd = kprocess::current_files().add_file(created.file, true)?;
    if let Err(error) = params.write_vm(value) {
        let _ = kprocess::current_files().close_file(fd);
        return Err(error.into());
    }
    Ok(fd as isize)
}

/// Submits SQEs and optionally waits for completions.
///
/// `flags` accepts only `IORING_ENTER_GETEVENTS`. A signal mask is rejected
/// only when `GETEVENTS` requests the wait this subset cannot service; Linux
/// ignores the mask otherwise.
///
/// # Execution context
///
/// Requires a current process user thread: the ring descriptor is resolved in
/// the caller's fd table. It may sleep while worker spawn or a `GETEVENTS`
/// wait blocks, so it must not run in interrupt context or while holding a
/// spinlock or an IRQ/preemption disabled guard.
pub(crate) fn sys_io_uring_enter(
    fd: i32,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    sig: UserConstPtr<k_sigset>,
    _sigsetsize: usize,
) -> KResult<isize> {
    if flags & !IORING_ENTER_GETEVENTS != 0 {
        return Err(KError::InvalidInput);
    }
    let wait = flags & IORING_ENTER_GETEVENTS != 0;
    if wait && !sig.is_null() {
        return Err(KError::InvalidInput);
    }

    let file = kprocess::current_files().get_file(fd)?;
    let ring = AsyncioFile::from_file(&file).map_err(|_| KError::OperationNotSupported)?;
    let submitted = ring.enter(to_submit, if wait { min_complete } else { 0 }, None)?;
    Ok(submitted as isize)
}

unittest::path_mod_test!(tests, "../../ut-tests/io_mpx/io_uring.rs");
