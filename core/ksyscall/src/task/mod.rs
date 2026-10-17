// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task and process management syscalls.
//!
//! This module implements process and thread management operations including:
//! - Process creation and execution (fork, clone, execve, etc.)
//! - Process termination (exit, kill, etc.)
//! - Process control (wait, ptrace, etc.)
//! - Thread management (thread creation, scheduling, etc.)
//! - Job control and process groups (setpgid, getpgrp, etc.)

mod clone;
mod clone3;
mod cpu_time;
mod credentials;
mod ctl;
mod execve;
mod exit;
mod ids;
mod job;
mod limits;
mod namespace;
mod pidfd;
mod restart;
mod rusage;
mod sched;
mod seccomp;
mod signal;
mod thread;
mod umask;
mod wait;

unittest::path_mod_test!(
    mod kernel_task_root,
    "../../ut-tests/task/kernel_task_root.rs"
);

pub(crate) use self::{
    clone::*, clone3::*, cpu_time::*, credentials::*, ctl::*, execve::*, exit::*, ids::*, job::*,
    limits::*, namespace::*, pidfd::*, restart::*, rusage::*, sched::*, seccomp::*, signal::*,
    thread::*, umask::*, wait::*,
};
