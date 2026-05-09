// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll syscalls.
//!
//! This module implements epoll I/O multiplexing operations including:
//! - Epoll instance creation (epoll_create, epoll_create1, etc.)
//! - Epoll event management (epoll_ctl, etc.)
//! - Event waiting (epoll_wait, epoll_pwait, etc.)
//! - High-performance event notification

use alloc::sync::Arc;
use core::time::Duration;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kpoll::IoEvents;
use kservices::{
    mm::{UserConstPtr, UserPtr},
    nullable,
    signal::with_replacen_blocked,
};
use ksignal::SignalSet;
use ktask::future::{self, block_on, poll_io};
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, epoll_event, timespec,
};
use posix_signal::check_sigset_size;
use posix_types::TimeValueLike;

use crate::file::epoll::{Epoll, EpollEvent, EpollFlags};

bitflags! {
    /// Flags for the `epoll_create` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollCreateFlags: u32 {
        const CLOEXEC = EPOLL_CLOEXEC;
    }
}

/// Create an epoll instance for efficient I/O event multiplexing
pub fn sys_epoll_create1(flags: u32) -> KResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");
    let resources = kthread::current_resources();
    resources
        .add_file_like(
            Arc::new(Epoll::new()),
            flags.contains(EpollCreateFlags::CLOEXEC),
        )
        .map(|fd| fd as isize)
}

/// Control the epoll instance: add, modify, or delete event subscriptions
pub fn sys_epoll_ctl(
    epfd: i32,
    op: u32,
    fd: i32,
    event: UserConstPtr<epoll_event>,
) -> KResult<isize> {
    let epoll = kthread::current_resources().get_file_like_as::<Epoll>(epfd)?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");

    let parse_event = || -> KResult<(EpollEvent, EpollFlags)> {
        let event = event.get_as_ref()?;
        let events = IoEvents::from_bits_truncate(event.events);
        let flags =
            EpollFlags::from_bits(event.events & !events.bits()).ok_or(KError::InvalidInput)?;
        Ok((
            EpollEvent {
                events,
                user_data: event.data,
            },
            flags,
        ))
    };
    match op {
        EPOLL_CTL_ADD => {
            let (event, flags) = parse_event()?;
            epoll.add(fd, event, flags)?;
        }
        EPOLL_CTL_MOD => {
            let (event, flags) = parse_event()?;
            epoll.modify(fd, event, flags)?;
        }
        EPOLL_CTL_DEL => {
            epoll.delete(fd)?;
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(0)
}

/// Wait for events on the epoll instance, with optional signal mask replacement
fn do_epoll_wait(
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: Option<Duration>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> KResult<isize> {
    check_sigset_size(sigsetsize)?;
    debug!("sys_epoll_wait <= epfd: {epfd}, maxevents: {maxevents}, timeout: {timeout:?}");

    let epoll = kthread::current_resources().get_file_like_as::<Epoll>(epfd)?;

    if maxevents <= 0 {
        return Err(KError::InvalidInput);
    }
    let events = events.get_as_mut_slice(maxevents as usize)?;

    with_replacen_blocked(
        nullable!(sigmask.get_as_ref())?.copied(),
        || match block_on(future::timeout(
            timeout,
            poll_io(epoll.as_ref(), IoEvents::IN, false, || {
                epoll.poll_events(events)
            }),
        )) {
            Ok(r) => r.map(|n| n as _),
            Err(_) => Ok(0),
        },
    )
}

/// Wait for events with millisecond timeout and signal masking
pub fn sys_epoll_pwait(
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: i32,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = match timeout {
        -1 => None,
        t if t >= 0 => Some(Duration::from_millis(t as u64)),
        _ => return Err(KError::InvalidInput),
    };
    do_epoll_wait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
}

/// Wait for events with high-precision timeout and signal masking
pub fn sys_epoll_pwait2(
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = nullable!(timeout.get_as_ref())?
        .map(|ts| ts.try_into_time_value())
        .transpose()?;
    do_epoll_wait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
}
