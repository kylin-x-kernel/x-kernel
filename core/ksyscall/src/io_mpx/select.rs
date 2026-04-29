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
use core::{fmt, time::Duration};

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult};
use kpoll::IoEvents;
use kservices::{
    mm::{UserConstPtr, UserPtr},
    nullable,
    signal::with_replacen_blocked,
};
use ksignal::SignalSet;
use ktask::future::{self, block_on, poll_io};
use linux_raw_sys::general::*;
use osvm::{VirtMutPtr, VirtPtr};
use posix_types::TimeValueLike;

use super::FdPollSet;
use crate::{file::FD_TABLE, signal::check_sigset_size};

const FD_SETSIZE: usize = __FD_SETSIZE as usize;

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

/// Internal file descriptor set with a memory layout identical to user-space
/// `fd_set` / `__kernel_fd_set`, serving as both the transfer type (via
/// `read_vm` / `write_vm`) and the operation type (via `set` / `is_set` /
/// `clear`).
#[repr(C)]
#[derive(Clone, Copy, AnyBitPattern)]
pub(crate) struct FdSet {
    fds_bits: [usize; FD_SETSIZE / usize::BITS as usize],
}

impl FdSet {
    fn zeroed() -> Self {
        Self {
            fds_bits: [0; FD_SETSIZE / usize::BITS as usize],
        }
    }

    fn set(&mut self, fd: usize) {
        debug_assert!(fd < FD_SETSIZE);
        self.fds_bits[fd / usize::BITS as usize] |= 1 << (fd % usize::BITS as usize);
    }

    fn is_set(&self, fd: usize) -> bool {
        debug_assert!(fd < FD_SETSIZE);
        (self.fds_bits[fd / usize::BITS as usize] & (1 << (fd % usize::BITS as usize))) != 0
    }

    fn clear(&mut self) {
        self.fds_bits.fill(0);
    }

    /// Read an `FdSet` from user space. Returns `None` if the pointer is null.
    fn read_from_user(ptr: UserPtr<Self>, nfds: usize) -> KResult<Option<Self>> {
        if ptr.is_null() {
            return Ok(None);
        }
        let mut fdset: Self = ptr.address().as_ptr_of::<Self>().read_vm()?;
        let full_words = nfds / usize::BITS as usize;
        let remaining = nfds % usize::BITS as usize;
        if remaining > 0 && full_words < fdset.fds_bits.len() {
            fdset.fds_bits[full_words] &= (1usize << remaining) - 1;
        }
        for w in fdset
            .fds_bits
            .iter_mut()
            .skip(full_words + usize::from(remaining > 0))
        {
            *w = 0;
        }
        Ok(Some(fdset))
    }

    /// Write this `FdSet` back to user space.
    fn write_to_user(&self, ptr: UserPtr<Self>) -> KResult<()> {
        if ptr.is_null() {
            return Ok(());
        }
        ptr.address().as_mut_ptr_of::<Self>().write_vm(*self)?;
        Ok(())
    }
}

impl fmt::Debug for FdSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries((0..FD_SETSIZE).filter(|&i| self.is_set(i)))
            .finish()
    }
}

/// Monitor multiple file descriptors for readability, writability, or exceptional conditions
fn do_select(
    nfds: u32,
    readfds: UserPtr<FdSet>,
    writefds: UserPtr<FdSet>,
    exceptfds: UserPtr<FdSet>,
    timeout: Option<Duration>,
    sigmask: UserConstPtr<SignalSetWithSize>,
) -> KResult<isize> {
    let nfds = nfds as usize;
    if nfds > FD_SETSIZE {
        return Err(KError::InvalidInput);
    }
    let sigmask = if let Some(sigmask) = nullable!(sigmask.get_as_ref())? {
        check_sigset_size(sigmask.sigsetsize)?;
        let set = sigmask.set;
        nullable!(set.get_as_ref())?
    } else {
        None
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

    let fd_table = FD_TABLE.read();
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
            .inner
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

    let result = with_replacen_blocked(sigmask.copied(), || {
        match block_on(future::timeout(
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
        }
    });

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
        nullable!(timeout.get_as_ref())?
            .map(|it| it.try_into_time_value())
            .transpose()?,
        0.into(),
    )
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SignalSetWithSize {
    set: UserConstPtr<SignalSet>,
    sigsetsize: usize,
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
        nullable!(timeout.get_as_ref())?
            .map(|ts| ts.try_into_time_value())
            .transpose()?,
        sigmask,
    )
}
