// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{fmt, string::String};

use ksignal::api::SignalMasks;
use ktask::{TaskInner, TaskState};

use crate::AsThread;

/// Number of nanoseconds in one second.
const NSEC_PER_SEC: u64 = 1_000_000_000;
/// User-visible clock-tick frequency used by `/proc/[pid]/stat` (USER_HZ).
const USER_HZ: u64 = 100;
/// Linux `TASK_COMM_LEN`, including the terminating NUL in the kernel buffer.
const TASK_COMM_LEN: usize = 16;
/// Linux's obsolete `/proc/[pid]/stat` signal fields expose only the low 31
/// bits of the 64-bit masks, so signals 32-64 cannot leak into those columns.
const STAT_SIGNAL_MASK: u64 = 0x7fff_ffff;

/// Converts a monotonic nanoseconds-since-boot timestamp into the
/// `/proc/[pid]/stat` `starttime` representation (clock ticks since boot).
fn starttime_ticks(start_time_ns: u64) -> u64 {
    start_time_ns / (NSEC_PER_SEC / USER_HZ)
}

/// Represents the `/proc/[pid]/stat` file.
#[derive(Default)]
pub struct TaskStat {
    /// Thread ID (Linux `stat` field 1).
    pub pid: u32,
    /// Task name (`comm`), not the executable path. It is truncated to Linux's
    /// 16-byte `TASK_COMM_LEN` buffer (at most 15 visible bytes plus NUL).
    pub comm: String,
    /// Process state.
    pub state: char,
    /// Parent process ID.
    pub ppid: u32,
    /// Process group ID.
    pub pgrp: u32,
    /// Session ID.
    pub session: u32,
    /// Controlling terminal of the process.
    pub tty_nr: u32,
    /// Foreground process group ID of the terminal.
    pub tpgid: u32,
    /// Kernel flags for the process.
    pub flags: u32,
    /// Minor faults the process has made.
    pub minflt: u64,
    /// Minor faults made by the process's waited-for children.
    pub cminflt: u64,
    /// Major faults the process has made.
    pub majflt: u64,
    /// Major faults made by the process's waited-for children.
    pub cmajflt: u64,
    /// User mode time in jiffies.
    pub utime: u64,
    /// Kernel mode time in jiffies.
    pub stime: u64,
    /// User mode time of waited-for children in jiffies.
    pub cutime: u64,
    /// Kernel mode time of waited-for children in jiffies.
    pub cstime: u64,
    /// Process priority.
    pub priority: i32,
    /// Nice value.
    pub nice: i32,
    /// Number of unreleased threads in this process, including a group leader
    /// that has exited but has not yet been reaped.
    pub num_threads: u32,
    /// Obsolete (always 0 in Linux).
    pub itrealvalue: u32,
    /// Start time since system boot (jiffies).
    pub starttime: u64,
    /// Virtual memory size in bytes, or `None` when the task has no live
    /// address space (for example, a zombie).
    ///
    /// `/proc/[pid]/stat` always prints this positional field as a number
    /// (`None` becomes `0`), while `/proc/[pid]/status` omits `VmSize` when it
    /// is `None`, matching Linux's `if (mm)` guard.
    pub vsize: Option<u64>,
    /// Resident set size in pages.
    ///
    /// X-Kernel has no resident-page accounting yet, so this positional field
    /// remains zero. It is a placeholder, not a measured value; `status` omits
    /// `VmRSS` for the same reason.
    pub rss: i64,
    /// Soft limit for RSS.
    pub rsslim: u64,
    /// Address above which program text can run.
    pub start_code: u64,
    /// Address below which program text can run.
    pub end_code: u64,
    /// Address of the start of the stack.
    pub start_stack: u64,
    /// Current value of ESP (obsolete).
    pub kstk_esp: u64,
    /// Current value of EIP (obsolete).
    pub kstk_eip: u64,
    /// Pending signals private to this thread.
    pub signal: u64,
    /// Blocked signals.
    pub blocked: u64,
    /// Ignored signals.
    pub sigignore: u64,
    /// Caught signals.
    pub sigcatch: u64,
    /// Channel in which the process is waiting.
    pub wchan: u64,
    /// Swapped-out memory (obsolete).
    pub nswap: u64,
    /// Swapped-out memory of children (obsolete).
    pub cnswap: u64,
    /// Signal sent to the parent when this thread dies.
    ///
    /// Linux reports `-1` for every non-leader thread: only the group leader
    /// owns the parent notification contract. A leader created without one
    /// (for example the initial task) reports `0`.
    pub exit_signal: i32,
    /// Last CPU number executed on.
    pub processor: u32,
    /// Realtime scheduling priority.
    pub rt_priority: u32,
    /// Scheduling policy.
    pub policy: u32,
    /// Aggregated block I/O delays.
    pub delayacct_blkio_ticks: u64,
    /// Guest time of the process.
    pub guest_time: u64,
    /// Guest time of children.
    pub cguest_time: u64,
    /// Address above which initialized and uninitialized data are placed.
    pub start_data: u64,
    /// Address below which initialized and uninitialized data are placed.
    pub end_data: u64,
    /// Address above which heap can be expanded with `brk`.
    pub start_brk: u64,
    /// Address above which command-line arguments are placed.
    pub arg_start: u64,
    /// Address below which command-line arguments are placed.
    pub arg_end: u64,
    /// Address above which environment is placed.
    pub env_start: u64,
    /// Address below which environment is placed.
    pub env_end: u64,
    /// The thread's exit status.
    pub exit_code: i32,
}

impl TaskStat {
    /// Creates a new [`TaskStat`] from a [`TaskInner`].
    ///
    /// # Panics
    ///
    /// Panics when the task has no Thread runtime.
    pub fn from_thread(task: &TaskInner) -> Self {
        Self::sample_thread(task).0
    }

    /// Samples the stat view and the signal masks used to fill it.
    ///
    /// A caller that also renders `status` can use the returned
    /// [`SignalMasks`] for `ShdPnd`, so `SigPnd`, `ShdPnd`, and `SigBlk` all
    /// describe one `SigLock` critical section instead of three independent
    /// samples. Keeping the masks tied to the stat snapshot avoids accepting
    /// masks sampled from a different task.
    ///
    /// # Panics
    ///
    /// Panics when the task has no Thread runtime.
    #[must_use]
    pub fn sample_thread(task: &TaskInner) -> (Self, SignalMasks) {
        let masks = task.as_thread().signal_manager().masks();
        (Self::from_thread_with_signal_masks(task, masks), masks)
    }

    fn from_thread_with_signal_masks(task: &TaskInner, masks: SignalMasks) -> Self {
        let thread = task.as_thread();
        let proc = thread.process();

        let pid = thread.tid();
        let mut name = [0; TASK_COMM_LEN];
        // Linux reserves the final byte for the terminating NUL, so at most
        // `TASK_COMM_LEN - 1` bytes of the task name are visible.
        let name_len = task.copy_name(&mut name[..TASK_COMM_LEN - 1]);
        // `copy_name` truncates on a UTF-8 boundary, so this cannot fail; keep
        // the fallback empty rather than turning a name bug into a panic while
        // rendering procfs.
        let comm = String::from(core::str::from_utf8(&name[..name_len]).unwrap_or_default());
        let state = match task.state() {
            TaskState::Running | TaskState::Ready => 'R',
            TaskState::Blocked
            | TaskState::BlockedInterruptible
            | TaskState::BlockedKillable
            | TaskState::Frozen => 'S',
            TaskState::Exited => 'Z',
        };
        let ppid = proc.parent().map_or(0, |p| p.pid());
        let pgrp = proc.group().pgid();
        let session = proc.group().session().sid();
        let nice = thread.nice();
        // Linux reports `starttime` in clock ticks since boot with USER_HZ=100
        // (i.e. nanoseconds / 10_000_000). Runtimes such as Go's (runc) compare
        // it against persisted state to decide whether a process is still the
        // one they spawned; tasks started inside the first tick still report 0,
        // matching Linux's own truncation.
        let starttime = starttime_ticks(task.start_time_ns());
        let (sigignore, sigcatch) = thread.signal_manager().disposition_masks();
        // Keep "no address space" distinct from a mapped size of zero so
        // `status` can apply Linux's `if (mm)` rule; `stat` still renders the
        // positional field as zero.
        let vsize = proc.virtual_memory_size();
        // `clone(CLONE_THREAD)` tasks do not notify a parent individually and
        // Linux marks their `exit_signal` as -1. The group leader keeps the
        // process-wide wait contract, including a custom clone signal.
        let exit_signal = if pid == proc.pid() {
            proc.exit_signal().map_or(0, |signo| signo as i32)
        } else {
            -1
        };
        Self {
            pid,
            comm,
            state,
            ppid,
            pgrp,
            session,
            priority: nice.proc_stat_priority(),
            nice: nice.as_i32(),
            // Linux `get_nr_threads()` keeps counting the zombie leader until
            // wait/autoreap removes the process identity, even though that
            // leader is no longer a published/schedulable task.
            num_threads: proc.unreleased_thread_count() as u32,
            starttime,
            vsize,
            signal: masks.private_pending().bits(),
            blocked: masks.blocked().bits(),
            sigignore: sigignore.bits(),
            sigcatch: sigcatch.bits(),
            exit_signal,
            exit_code: proc.exit_code(),
            ..Default::default()
        }
    }
}

impl fmt::Display for TaskStat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            pid,
            comm,
            state,
            ppid,
            pgrp,
            session,
            tty_nr,
            tpgid,
            flags,
            minflt,
            cminflt,
            majflt,
            cmajflt,
            utime,
            stime,
            cutime,
            cstime,
            priority,
            nice,
            num_threads,
            itrealvalue,
            starttime,
            vsize,
            rss,
            rsslim,
            start_code,
            end_code,
            start_stack,
            kstk_esp,
            kstk_eip,
            signal,
            blocked,
            sigignore,
            sigcatch,
            wchan,
            nswap,
            cnswap,
            exit_signal,
            processor,
            rt_priority,
            policy,
            delayacct_blkio_ticks,
            guest_time,
            cguest_time,
            start_data,
            end_data,
            start_brk,
            arg_start,
            arg_end,
            env_start,
            env_end,
            exit_code,
        } = self;
        let vsize = vsize.unwrap_or_default();
        // These four fields predate the 64-bit signal ABI. Linux still prints
        // `sig[0] & 0x7fffffff` for them, so signals 32-64 (and bit 31) must not
        // leak into the legacy decimal columns. `status` uses the full masks.
        let signal = signal & STAT_SIGNAL_MASK;
        let blocked = blocked & STAT_SIGNAL_MASK;
        let sigignore = sigignore & STAT_SIGNAL_MASK;
        let sigcatch = sigcatch & STAT_SIGNAL_MASK;
        writeln!(
            f,
            "{pid} ({comm}) {state} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} {minflt} \
             {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} \
             {num_threads} {itrealvalue} {starttime} {vsize} {rss} {rsslim} {start_code} \
             {end_code} {start_stack} {kstk_esp} {kstk_eip} {signal} {blocked} {sigignore} \
             {sigcatch} {wchan} {nswap} {cnswap} {exit_signal} {processor} {rt_priority} {policy} \
             {delayacct_blkio_ticks} {guest_time} {cguest_time} {start_data} {end_data} \
             {start_brk} {arg_start} {arg_end} {env_start} {env_end} {exit_code}",
        )
    }
}

unittest::path_mod_test!(tests_stat, "../ut-tests/stat.rs");
