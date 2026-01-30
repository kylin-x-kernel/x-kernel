use alloc::{borrow::ToOwned, fmt, string::String};

use kerrno::KResult;
use ksignal::Signo;
use ktask::{TaskInner, TaskState};

use crate::task::AsThread;

/// Represents the `/proc/[pid]/stat` file.
///
/// See ['https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html'] for details.
#[derive(Default)]
pub struct TaskStat {
    /// Process ID.
    pub pid: u32,
    /// Filename of the executable (limited to 16 chars).
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
    pub priority: u32,
    /// Nice value.
    pub nice: u32,
    /// Number of threads in this process.
    pub num_threads: u32,
    /// Obsolete (always 0 in Linux).
    pub itrealvalue: u32,
    /// Start time since system boot (jiffies).
    pub starttime: u64,
    /// Virtual memory size in bytes.
    pub vsize: u64,
    /// Resident Set Size.
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
    /// Pending signals.
    pub signal: u32,
    /// Blocked signals.
    pub blocked: u32,
    /// Ignored signals.
    pub sigignore: u32,
    /// Caught signals.
    pub sigcatch: u32,
    /// Channel in which the process is waiting.
    pub wchan: u64,
    /// Swapped-out memory (obsolete).
    pub nswap: u64,
    /// Swapped-out memory of children (obsolete).
    pub cnswap: u64,
    /// Signal sent to parent when we die.
    pub exit_signal: u8,
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
    /// Address above which program initialized and uninitialized data are placed.
    pub start_data: u64,
    /// Address below which program initialized and uninitialized data are placed.
    pub end_data: u64,
    /// Address above which program heap can be expanded with brk.
    pub start_brk: u64,
    /// Address above which program command-line arguments are placed.
    pub arg_start: u64,
    /// Address below which program command-line arguments are placed.
    pub arg_end: u64,
    /// Address above which program environment is placed.
    pub env_start: u64,
    /// Address below which program environment is placed.
    pub env_end: u64,
    /// The thread's exit status.
    pub exit_code: i32,
}

impl TaskStat {
    /// Create a new [`TaskStat`] from a [`KtaskRef`].
    pub fn from_thread(task: &TaskInner) -> KResult<Self> {
        let thread = task.as_thread();
        let proc_data = &thread.proc_data;
        let proc = &proc_data.proc;

        let pid = proc.pid();
        let comm = task.name();
        let comm = comm[..comm.len().min(16)].to_owned();
        let state = match task.state() {
            TaskState::Running | TaskState::Ready => 'R',
            TaskState::Blocked => 'S',
            TaskState::Exited => 'Z',
        };
        let ppid = proc.parent().map_or(0, |p| p.pid());
        let pgrp = proc.group().pgid();
        let session = proc.group().session().sid();
        Ok(Self {
            pid,
            comm: comm.to_owned(),
            state,
            ppid,
            pgrp,
            session,
            num_threads: proc.threads().len() as u32,
            exit_signal: proc_data.exit_signal.unwrap_or(Signo::SIGCHLD) as u8,
            exit_code: proc.exit_code(),
            ..Default::default()
        })
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
