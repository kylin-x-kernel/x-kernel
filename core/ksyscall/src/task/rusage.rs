// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process and thread resource-usage syscalls.

use kerrno::{KError, KResult};
use kprocess::{Process, Thread};
use ktime_types::TimeSpan;
use linux_raw_sys::general::{__kernel_old_timeval, rusage};
use posix_types::{TimeSpanLike, UserPtr};

#[derive(Default)]
struct Rusage {
    utime: TimeSpan,
    stime: TimeSpan,
}

impl Rusage {
    fn from_thread(thread: &Thread) -> Self {
        let (utime, stime) = thread.sample_cpu_time();
        Self { utime, stime }
    }

    #[cfg(unittest)]
    fn collate(self, other: Self) -> Self {
        Self {
            utime: self.utime.saturating_add(other.utime),
            stime: self.stime.saturating_add(other.stime),
        }
    }
}

impl From<Rusage> for rusage {
    fn from(value: Rusage) -> Self {
        Self {
            ru_utime: __kernel_old_timeval::from_time_span(value.utime),
            ru_stime: __kernel_old_timeval::from_time_span(value.stime),
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

fn self_rusage() -> Rusage {
    let process = kprocess::current_user_process();
    let (utime, stime) = process.process_cpu_times();
    Rusage { utime, stime }
}

fn children_rusage(process: &Process) -> Rusage {
    // Linux reports only waited-for children here. Live or merely zombie-but-not-reaped
    // children must not contribute until the parent successfully reaps them.
    let (utime, stime) = process.child_time();
    Rusage { utime, stime }
}

/// Returns resource usage information for the current process, its children, or the current
/// thread.
pub fn sys_getrusage(who: i32, usage: UserPtr<rusage>) -> KResult<isize> {
    const RUSAGE_SELF: i32 = linux_raw_sys::general::RUSAGE_SELF as i32;
    const RUSAGE_CHILDREN: i32 = linux_raw_sys::general::RUSAGE_CHILDREN;
    const RUSAGE_THREAD: i32 = linux_raw_sys::general::RUSAGE_THREAD as i32;

    let current_thread = kprocess::current_user_thread();
    let result = match who {
        RUSAGE_SELF => self_rusage(),
        RUSAGE_CHILDREN => children_rusage(current_thread.process()),
        RUSAGE_THREAD => Rusage::from_thread(&current_thread),
        _ => return Err(KError::InvalidInput),
    };

    usage.write_vm(result.into())?;
    Ok(0)
}

#[cfg(unittest)]
mod tests {
    use ktime_types::TimeSpan;
    use unittest::def_test;

    use super::Rusage;

    #[def_test]
    fn rusage_collate_adds_user_and_system_time() {
        let a = Rusage {
            utime: TimeSpan::new(1, 500_000_000),
            stime: TimeSpan::new(2, 0),
        };
        let b = Rusage {
            utime: TimeSpan::new(0, 500_000_000),
            stime: TimeSpan::new(3, 0),
        };

        let c = a.collate(b);

        assert_eq!(c.utime, TimeSpan::new(2, 0));
        assert_eq!(c.stime, TimeSpan::new(5, 0));
    }

    #[def_test]
    fn rusage_collate_default_is_identity() {
        let a = Rusage {
            utime: TimeSpan::new(7, 0),
            stime: TimeSpan::new(8, 0),
        };

        let c = a.collate(Rusage::default());

        assert_eq!(c.utime, TimeSpan::new(7, 0));
        assert_eq!(c.stime, TimeSpan::new(8, 0));
    }

    #[def_test]
    fn rusage_collate_normalizes_nanoseconds_into_seconds() {
        let a = Rusage {
            utime: TimeSpan::new(0, 600_000_000),
            stime: TimeSpan::new(0, 0),
        };
        let b = Rusage {
            utime: TimeSpan::new(0, 600_000_000),
            stime: TimeSpan::new(0, 0),
        };

        let c = a.collate(b);

        assert_eq!(c.utime, TimeSpan::new(1, 200_000_000));
        assert_eq!(c.stime, TimeSpan::new(0, 0));
    }
}
