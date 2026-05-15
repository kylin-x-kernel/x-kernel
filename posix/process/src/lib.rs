// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX process and thread syscall implementations.
//!
//! - Process and thread IDs (`getpid`, `getppid`, `gettid`, `set_tid_address`)
//! - Job control (`getsid`, `setsid`, `getpgid`, `setpgid`)
//! - CPU time and usage statistics (`times`, `getrusage`)
//! - Resource limits (`getrlimit`, `setrlimit`, `prlimit64`)
//! - File-mode creation mask (`umask`)

#![no_std]

mod cpu_time;
mod ids;
mod job_control;
mod limits;
mod rusage;
mod thread;
mod umask;

pub use cpu_time::sys_times;
pub use ids::{sys_getpid, sys_getppid};
pub use job_control::{sys_getpgid, sys_getsid, sys_setpgid, sys_setsid};
pub use limits::{sys_getrlimit, sys_prlimit64, sys_setrlimit};
pub use rusage::sys_getrusage;
pub use thread::{sys_gettid, sys_set_tid_address};
pub use umask::sys_umask;
