// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll syscalls.

use alloc::{sync::Arc, vec};
use core::time::Duration;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kpoll::IoEvents;
use kservices::signal::with_replacen_blocked;
use ksignal::SignalSet;
use ktask::future::{self, block_on, poll_io};
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, epoll_event, timespec,
};
use posix_signal::check_sigset_size;
use posix_types::{TimeValueLike, UserConstPtr, UserPtr};

use super::{Epoll, EpollEvent, EpollFlags};

fn read_user_value<T>(ptr: UserConstPtr<T>) -> KResult<T> {
    // SAFETY: The caller chooses a syscall ABI type that is consumed by value at
    // the syscall boundary, so copying bytes out of user memory and assuming the
    // resulting value is initialized matches Linux `copy_from_user` semantics.
    unsafe {
        ptr.read_uninit()
            .map(|value| value.assume_init())
            .map_err(Into::into)
    }
}

bitflags! {
    /// Flags for the `epoll_create1` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollCreateFlags: u32 {
        const CLOEXEC = EPOLL_CLOEXEC;
    }
}

/// Creates an epoll instance.
pub fn sys_epoll_create1(flags: u32) -> KResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");

    kthread::current_resources()
        .add_file_like(
            Arc::new(Epoll::new()),
            flags.contains(EpollCreateFlags::CLOEXEC),
        )
        .map(|fd| fd as isize)
}

/// Adds, modifies, or removes an epoll interest.
pub fn sys_epoll_ctl(
    epfd: i32,
    op: u32,
    fd: i32,
    event: UserConstPtr<epoll_event>,
) -> KResult<isize> {
    let epoll = kthread::current_resources().get_file_like_as::<Epoll>(epfd)?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");

    let parse_event = || -> KResult<(EpollEvent, EpollFlags)> {
        let event = read_user_value(event)?;
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

    if maxevents <= 0 {
        return Err(KError::InvalidInput);
    }

    let epoll = kthread::current_resources().get_file_like_as::<Epoll>(epfd)?;
    // `posix-types::UserPtr` models copy-to-user semantics rather than a borrowed
    // mutable user slice, so we stage ready events in kernel memory and copy them
    // back once polling completes.
    let mut output = vec![epoll_event { events: 0, data: 0 }; maxevents as usize];
    let sigmask = sigmask.check_non_null().map(read_user_value).transpose()?;

    let ready = with_replacen_blocked(sigmask, || {
        match block_on(future::timeout(
            timeout,
            poll_io(epoll.as_ref(), IoEvents::IN, false, || {
                epoll.poll_events(&mut output)
            }),
        )) {
            Ok(r) => r,
            Err(_) => Ok(0),
        }
    })?;

    events.write_vm_slice(&output[..ready])?;
    Ok(ready as isize)
}

/// Waits for epoll events with a millisecond timeout.
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

/// Waits for epoll events with a high-precision timeout.
pub fn sys_epoll_pwait2(
    epfd: i32,
    events: UserPtr<epoll_event>,
    maxevents: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<SignalSet>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = timeout
        .check_non_null()
        .map(read_user_value)
        .transpose()?
        .map(|timeout| timeout.try_into_time_value())
        .transpose()?;
    do_epoll_wait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
}
