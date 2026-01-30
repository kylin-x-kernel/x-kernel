//! Task and process management syscalls.
//!
//! This module implements process and thread management operations including:
//! - Process creation and execution (fork, clone, execve, etc.)
//! - Process termination (exit, kill, etc.)
//! - Process control (wait, ptrace, etc.)
//! - Thread management (thread creation, scheduling, etc.)
//! - Job control and process groups (setpgid, getpgrp, etc.)

mod clone;
mod ctl;
mod execve;
mod exit;
mod job;
mod schedule;
mod thread;
mod wait;

pub use self::{clone::*, ctl::*, execve::*, exit::*, job::*, schedule::*, thread::*, wait::*};
