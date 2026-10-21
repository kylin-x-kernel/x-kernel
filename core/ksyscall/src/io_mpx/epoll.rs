// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll syscalls.

use alloc::vec;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kfd_objects::epoll::{Epoll, EpollEvent, EpollFlags};
use kpoll::IoEvents;
use ktask::future::{self, block_on, poll_io};
use ktime_types::TimeSpan;
use linux_raw_sys::general::{
    EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, timespec,
};
use posix_types::{TimeSpanLike, UserConstPtr, UserPtr, k_epoll_event, k_sigset};

bitflags! {
    // Flags for the `epoll_create1` syscall.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollCreateFlags: u32 {
        const CLOEXEC = EPOLL_CLOEXEC;
    }
}

/// Creates an epoll instance.
pub(crate) fn sys_epoll_create1(flags: u32) -> KResult<isize> {
    let flags = EpollCreateFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    debug!("sys_epoll_create1 <= flags: {flags:?}");

    kprocess::current_files()
        .add_file(
            Epoll::new_file(kprocess::current_cred())?,
            flags.contains(EpollCreateFlags::CLOEXEC),
        )
        .map(|fd| fd as isize)
}

/// Adds, modifies, or removes an epoll interest.
pub(crate) fn sys_epoll_ctl(
    epfd: i32,
    op: u32,
    fd: i32,
    event: UserConstPtr<k_epoll_event>,
) -> KResult<isize> {
    let epoll_file = kprocess::current_files().get_file(epfd)?;
    let epoll = Epoll::from_file(&epoll_file)?;
    let file = kprocess::current_files().get_file(fd)?;
    debug!("sys_epoll_ctl <= epfd: {epfd}, op: {op}, fd: {fd}");

    let parse_event = || -> KResult<(EpollEvent, EpollFlags)> {
        let event = event.read_vm()?;
        let events = IoEvents::from_bits_truncate(event.events());
        let flags =
            EpollFlags::from_bits(event.events() & !events.bits()).ok_or(KError::InvalidInput)?;
        Ok((
            EpollEvent {
                events,
                user_data: event.data(),
            },
            flags,
        ))
    };

    match op {
        EPOLL_CTL_ADD => {
            let (event, flags) = parse_event()?;
            epoll.add(fd, file.clone(), event, flags)?;
        }
        EPOLL_CTL_MOD => {
            let (event, flags) = parse_event()?;
            epoll.modify(fd, file.clone(), event, flags)?;
        }
        EPOLL_CTL_DEL => {
            epoll.delete(fd, file)?;
        }
        _ => return Err(KError::InvalidInput),
    }
    Ok(0)
}

fn do_epoll_wait(
    epfd: i32,
    events: UserPtr<k_epoll_event>,
    maxevents: i32,
    timeout: Option<TimeSpan>,
    sigmask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    posix_types::check_sigset_size(sigsetsize)?;
    debug!("sys_epoll_wait <= epfd: {epfd}, maxevents: {maxevents}, timeout: {timeout:?}");

    if maxevents <= 0 {
        return Err(KError::InvalidInput);
    }

    let epoll_file = kprocess::current_files().get_file(epfd)?;
    let epoll = Epoll::from_file(&epoll_file)?;
    // `posix-types::UserPtr` models copy-to-user semantics rather than a borrowed
    // mutable user slice, so we stage ready events in kernel memory and copy them
    // back once polling completes.
    let mut output = vec![k_epoll_event::default(); maxevents as usize];
    let sigmask = sigmask
        .check_non_null()
        .map(UserConstPtr::read_vm)
        .transpose()?;

    let ready =
        kprocess::current_user_thread().with_temp_blocked(sigmask.map(Into::into), || {
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
///
/// Any negative timeout waits indefinitely; zero polls without waiting.
pub(crate) fn sys_epoll_pwait(
    epfd: i32,
    events: UserPtr<k_epoll_event>,
    maxevents: i32,
    timeout: i32,
    sigmask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = if timeout < 0 {
        None
    } else {
        Some(TimeSpan::from_millis(timeout as u64))
    };
    do_epoll_wait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
}

/// Waits for epoll events with a high-precision timeout.
pub(crate) fn sys_epoll_pwait2(
    epfd: i32,
    events: UserPtr<k_epoll_event>,
    maxevents: i32,
    timeout: UserConstPtr<timespec>,
    sigmask: UserConstPtr<k_sigset>,
    sigsetsize: usize,
) -> KResult<isize> {
    let timeout = timeout
        .check_non_null()
        .map(UserConstPtr::read_vm)
        .transpose()?
        .map(|timeout| timeout.try_into_time_span())
        .transpose()?;
    do_epoll_wait(epfd, events, maxevents, timeout, sigmask, sigsetsize)
}

unittest::path_mod_test!(
    tests_epoll_padding,
    "../../ut-tests/io_mpx/epoll/tests_epoll_padding.rs"
);

unittest::path_mod_test!(
    tests_epoll_negative_timeout,
    "../../ut-tests/io_mpx/epoll/tests_epoll_negative_timeout.rs"
);
