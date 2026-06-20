// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process and thread resource-usage syscalls.

use kerrno::{KError, KResult};
use khal::time::TimeValue;
use kprocess::Process;
use kthread::{AsThread, ProcessState, Thread};
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use posix_types::{TimeValueLike, UserPtr};

#[derive(Default)]
struct Rusage {
    utime: TimeValue,
    stime: TimeValue,
}

impl Rusage {
    fn from_thread(thread: &Thread) -> Self {
        let (utime, stime) = thread.sample_cpu_time();
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
        Self {
            ru_utime: __kernel_old_timeval::from_time_value(value.utime),
            ru_stime: __kernel_old_timeval::from_time_value(value.stime),
            ru_maxrss: 0,
            ru_ixrss: 0,
            ru_idrss: 0,
            ru_isrss: 0,
            ru_minflt: 0,
            ru_majflt: 0,
            ru_nswap: 0,
            ru_inblock: 0,
            ru_oublock: 0,
            ru_msgsnd: 0,
            ru_msgrcv: 0,
            ru_nsignals: 0,
            ru_nvcsw: 0,
            ru_nivcsw: 0,
        }
    }
}

fn self_rusage(proc: &Process) -> Rusage {
    proc.threads()
        .into_iter()
        .fold(Rusage::default(), |acc, tid| {
            if let Ok(task) = kthread::get_task(tid) {
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
                if let Ok(task) = kthread::get_task(tid) {
                    acc.collate(Rusage::from_thread(task.as_thread()))
                } else {
                    acc
                }
            })
        });

    reaped.collate(live)
}

/// Returns resource usage information for the current process, its children, or the current
/// thread.
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
