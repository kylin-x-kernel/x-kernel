// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FD-backed kernel object implementations.
//!
//! This crate owns kernel objects that are exposed through the process fd table
//! but are not fundamentally VFS path objects. Syscall ABI adapters live in
//! `ksyscall`; this crate owns the object state, invariants, and file operation
//! behavior. Start with an object's `new_file` constructor after anonymous-inode
//! filesystem initialization, then install the returned file through the fd owner.
//! The backend can also be used directly for kernel readiness checks:
//!
//! ```no_run
//! use kfd_objects::eventfd::EventFd;
//! use kpoll::{IoEvents, Pollable};
//!
//! // Requires the kernel allocator and poll runtime.
//! let counter = EventFd::new(1, false);
//! assert!(counter.poll().contains(IoEvents::IN));
//! ```

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

pub mod epoll;
pub mod eventfd;
pub mod inotify;
pub mod signalfd;
pub mod timerfd;
