// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Select syscalls.
//!
//! This module implements traditional select I/O multiplexing including:
//! - Select operations (select, pselect6, etc.)
//! - File descriptor set management
//! - Timeout and signal handling

use alloc::vec::Vec;

use kerrno::{KError, KResult};
use kpoll::IoEvents;
use ktask::future::{self, block_on, poll_io};
use ktime_types::TimeSpan;
use linux_raw_sys::general::*;
use posix_types::{FdSet, SignalSetWithSize, TimeSpanLike, UserConstPtr, UserPtr};

use super::FdPollSet;

const POLLIN_SET: IoEvents = IoEvents::IN
    .union(IoEvents::RDNORM)
    .union(IoEvents::RDBAND)
    .union(IoEvents::HUP)
    .union(IoEvents::ERR);
const POLLOUT_SET: IoEvents = IoEvents::OUT
    .union(IoEvents::WRNORM)
    .union(IoEvents::WRBAND)
    .union(IoEvents::ERR);
const POLLEX_SET: IoEvents = IoEvents::PRI;

/// Monitor multiple file descriptors for readability, writability, or exceptional conditions
fn do_select(
    nfds: u32,
    readfds: UserPtr<FdSet>,
    writefds: UserPtr<FdSet>,
    exceptfds: UserPtr<FdSet>,
    timeout: Option<TimeSpan>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> KResult<isize> {
    let nfds = nfds as usize;
    if nfds > FdSet::FD_SETSIZE {
        return Err(KError::InvalidInput);
    }
    let sigmask = if sigmask.is_null() {
        None
    } else {
        let sigmask = sigmask.read_vm()?;
        posix_types::check_sigset_size(sigmask.sigsetsize())?;
        let set = sigmask.set();
        if set.is_null() {
            None
        } else {
            Some(set.read_vm()?)
        }
    };

    let read_set = FdSet::read_from_user(readfds, nfds)?;
    let write_set = FdSet::read_from_user(writefds, nfds)?;
    let except_set = FdSet::read_from_user(exceptfds, nfds)?;

    let read_fds = read_set.unwrap_or_else(FdSet::zeroed);
    let write_fds = write_set.unwrap_or_else(FdSet::zeroed);
    let except_fds = except_set.unwrap_or_else(FdSet::zeroed);

    debug!(
        "sys_select <= nfds: {nfds} sets: [read: {read_fds:?}, write: {write_fds:?}, except: \
         {except_fds:?}] timeout: {timeout:?}"
    );

    let resources = kprocess::current_resources();
    let fd_table = resources.fd_table()?;
    let fd_table = fd_table.read();
    let mut fds = Vec::with_capacity(nfds);
    let mut fd_indices = Vec::with_capacity(nfds);
    for fd in 0..nfds {
        let is_read = read_fds.is_set(fd);
        let is_write = write_fds.is_set(fd);
        let is_except = except_fds.is_set(fd);
        if !is_read && !is_write && !is_except {
            continue;
        }
        let f = fd_table
            .get(fd)
            .ok_or(KError::BadFileDescriptor)?
            .file()
            .clone();
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, is_read);
        events.set(IoEvents::OUT, is_write);
        events.set(IoEvents::PRI, is_except);
        fds.push((f, events));
        fd_indices.push(fd);
    }

    drop(fd_table);
    let fds = FdPollSet(fds);

    let mut res_in = FdSet::zeroed();
    let mut res_out = FdSet::zeroed();
    let mut res_ex = FdSet::zeroed();

    let result =
        kprocess::current_user_thread().with_temp_blocked(
            sigmask.map(Into::into),
            || match block_on(future::timeout(
                timeout,
                poll_io(&fds, IoEvents::empty(), false, || {
                    res_in.clear();
                    res_out.clear();
                    res_ex.clear();
                    let mut res = 0usize;
                    for ((fd, _), index) in fds.0.iter().zip(fd_indices.iter().copied()) {
                        let revents = fd.poll();
                        if read_fds.is_set(index) && revents.intersects(POLLIN_SET) {
                            res += 1;
                            res_in.set(index);
                        }
                        if write_fds.is_set(index) && revents.intersects(POLLOUT_SET) {
                            res += 1;
                            res_out.set(index);
                        }
                        if except_fds.is_set(index) && revents.intersects(POLLEX_SET) {
                            res += 1;
                            res_ex.set(index);
                        }
                    }
                    if res > 0 {
                        return Ok(res as _);
                    }

                    Err(KError::WouldBlock)
                }),
            )) {
                Ok(r) => r,
                Err(_) => Ok(0),
            },
        );

    // Only write back fd_sets on success; on error (e.g. EINTR) the
    // contents are unspecified per POSIX, so skip the unnecessary copy-out.
    if result.is_ok() {
        res_in.write_to_user(readfds)?;
        res_out.write_to_user(writefds)?;
        res_ex.write_to_user(exceptfds)?;
    }

    result
}

/// Select file descriptors with microsecond timeout
#[cfg(target_arch = "x86_64")]
pub fn sys_select(
    nfds: u32,
    readfds: UserPtr<FdSet>,
    writefds: UserPtr<FdSet>,
    exceptfds: UserPtr<FdSet>,
    timeout: UserConstPtr<timeval>,
) -> KResult<isize> {
    do_select(
        nfds,
        readfds,
        writefds,
        exceptfds,
        if timeout.is_null() {
            None
        } else {
            Some(timeout.read_vm()?.try_into_time_span()?)
        },
        0.into(),
    )
}

/// Select file descriptors with nanosecond timeout and signal masking
pub fn sys_pselect6(
    nfds: u32,
    readfds: UserPtr<FdSet>,
    writefds: UserPtr<FdSet>,
    exceptfds: UserPtr<FdSet>,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> KResult<isize> {
    do_select(
        nfds,
        readfds,
        writefds,
        exceptfds,
        if timeout.is_null() {
            None
        } else {
            Some(timeout.read_vm()?.try_into_time_span()?)
        },
        sigmask,
    )
}
