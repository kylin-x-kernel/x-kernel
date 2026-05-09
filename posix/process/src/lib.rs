// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process and thread syscall implementations.
//!
//! - Process and thread IDs (`getpid`, `getppid`, `gettid`)
//! - Thread-local process state hooks (`set_tid_address`, `umask`)
//! - Job control (`getsid`, `setsid`, `getpgid`, `setpgid`)
//! - Resource usage accounting (`getrusage`)

#![no_std]

use kerrno::{KError, KResult};
use khal::time::TimeValue;
use kprocess::{Pid, Process};
use ktask::current;
use kthread::{AsThread, ProcessState, Thread, get_process_group, get_process_state, get_task};
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use posix_types::{TimeValueLike, UserPtr};

/// Returns the process ID of the current process.
pub fn sys_getpid() -> KResult<isize> {
    Ok(kthread::current_thread().pid() as _)
}

/// Returns the parent process ID of the current process.
pub fn sys_getppid() -> KResult<isize> {
    let current_thread = kthread::current_thread();
    current_thread
        .process_state()
        .proc
        .parent()
        .ok_or(KError::NoSuchProcess)
        .map(|parent| parent.pid() as _)
}

/// Returns the thread ID of the current thread.
pub fn sys_gettid() -> KResult<isize> {
    Ok(current().id().as_u64() as _)
}

/// Sets the `clear_child_tid` pointer for the current thread.
pub fn sys_set_tid_address(clear_child_tid: usize) -> KResult<isize> {
    let current = current();
    kthread::current_thread().set_clear_child_tid(clear_child_tid);
    Ok(current.id().as_u64() as isize)
}

/// Returns the session ID of the given process.
pub fn sys_getsid(pid: Pid) -> KResult<isize> {
    Ok(get_process_state(pid)?.proc.group().session().sid() as _)
}

/// Creates a new session and makes the caller its leader.
pub fn sys_setsid() -> KResult<isize> {
    let current_thread = kthread::current_thread();
    let proc = &current_thread.process_state().proc;
    if get_process_group(proc.pid()).is_ok() {
        return Err(KError::OperationNotPermitted);
    }

    if let Some((session, _)) = proc.create_session() {
        Ok(session.sid() as _)
    } else {
        Ok(proc.pid() as _)
    }
}

/// Returns the process group ID of the given process.
pub fn sys_getpgid(pid: Pid) -> KResult<isize> {
    Ok(get_process_state(pid)?.proc.group().pgid() as _)
}

/// Sets the process group ID of the given process.
pub fn sys_setpgid(pid: Pid, pgid: Pid) -> KResult<isize> {
    let proc = &get_process_state(pid)?.proc;

    if pgid == 0 {
        proc.create_group();
    } else if !proc.move_to_group(&get_process_group(pgid)?) {
        return Err(KError::OperationNotPermitted);
    }

    Ok(0)
}

/// Sets the process umask and returns the previous value.
pub fn sys_umask(mask: u32) -> KResult<isize> {
    let old = kthread::current_thread()
        .process_state()
        .replace_umask(mask);
    Ok(old as isize)
}

#[derive(Default)]
struct Rusage {
    utime: TimeValue,
    stime: TimeValue,
}

impl Rusage {
    fn from_thread(thread: &Thread) -> Self {
        let (utime, stime) = thread.time.borrow().output();
        Self { utime, stime }
    }

    fn collate(mut self, other: Rusage) -> Self {
        self.utime += other.utime;
        self.stime += other.stime;
        self
    }
}

impl From<Rusage> for rusage {
    fn from(value: Rusage) -> Self {
        // SAFETY: `rusage` is a POD struct from `linux_raw_sys`. All-zeroes is a valid
        // initial state for every field (integer counters become 0, timevals become 0).
        let mut usage: rusage = unsafe { core::mem::zeroed() };
        usage.ru_utime = __kernel_old_timeval::from_time_value(value.utime);
        usage.ru_stime = __kernel_old_timeval::from_time_value(value.stime);
        usage
    }
}

fn self_rusage(proc: &Process) -> Rusage {
    proc.threads()
        .into_iter()
        .fold(Rusage::default(), |acc, tid| {
            if let Ok(task) = get_task(tid) {
                acc.collate(Rusage::from_thread(task.as_thread()))
            } else {
                acc
            }
        })
}

fn children_rusage(proc_state: &ProcessState) -> Rusage {
    // Accumulated reaped-children time + live children's current time.
    let (reaped_utime_ns, reaped_stime_ns) = proc_state.child_time_ns();
    let reaped = Rusage {
        utime: TimeValue::new(
            reaped_utime_ns as u64 / 1_000_000_000,
            (reaped_utime_ns as u64 % 1_000_000_000) as u32,
        ),
        stime: TimeValue::new(
            reaped_stime_ns as u64 / 1_000_000_000,
            (reaped_stime_ns as u64 % 1_000_000_000) as u32,
        ),
    };

    let live = proc_state
        .proc
        .children()
        .into_iter()
        .fold(Rusage::default(), |acc, child_proc| {
            child_proc.threads().into_iter().fold(acc, |acc, tid| {
                if let Ok(task) = get_task(tid) {
                    acc.collate(Rusage::from_thread(task.as_thread()))
                } else {
                    acc
                }
            })
        });

    reaped.collate(live)
}

/// Returns resource usage information for the current process, its children, or the current thread.
pub fn sys_getrusage(who: i32, usage: UserPtr<rusage>) -> KResult<isize> {
    const RUSAGE_SELF: i32 = linux_raw_sys::general::RUSAGE_SELF as i32;
    const RUSAGE_CHILDREN: i32 = linux_raw_sys::general::RUSAGE_CHILDREN;
    const RUSAGE_THREAD: i32 = linux_raw_sys::general::RUSAGE_THREAD as i32;

    let current_thread = kthread::current_thread();
    let proc_state = current_thread.process_state();
    let proc = &proc_state.proc;
    let result = match who {
        RUSAGE_SELF => self_rusage(proc),
        RUSAGE_CHILDREN => children_rusage(proc_state),
        RUSAGE_THREAD => Rusage::from_thread(&current_thread),
        _ => return Err(KError::InvalidInput),
    };

    usage.write_vm(result.into())?;
    Ok(0)
}
