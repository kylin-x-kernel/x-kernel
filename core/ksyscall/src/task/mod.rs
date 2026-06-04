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
mod ctl;
mod execve;
mod exit;
mod job;
mod pidfd;
mod sched;
mod thread;
mod wait;

#[cfg(target_arch = "x86_64")]
pub use self::thread::*;
pub use self::{clone::*, clone3::*, ctl::*, execve::*, exit::*, pidfd::*, sched::*, wait::*};
