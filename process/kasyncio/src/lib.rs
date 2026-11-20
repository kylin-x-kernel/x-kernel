// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Asynchronous I/O submission/completion infrastructure.
//!
//! This crate implements the kernel side of the X-Kernel async I/O model:
//! user/kernel shared submission and completion rings whose operations
//! execute on in-process kernel worker threads, so a blocking I/O no longer
//! parks the submitting user thread. Workers are sibling threads of the
//! submitting process, so user-memory access, credentials, and fd semantics
//! are identical to the synchronous syscall path. The synchronous syscall
//! surface stays untouched; async entry points are incremental.
//!
//! This is a Linux io_uring-compatible subset: single shared SQ/CQ mmap,
//! submission and completion draining. Non-goals: SQPOLL, registered
//! files/buffers, linked operations, and cancellation requests.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

/// User-visible async I/O ABI for the future submission/completion rings.
pub mod abi;
/// Per-process async context, ready/completion staging, and wake sources.
pub mod context;
// Worker internals stay crate-private; the file and setup layers are the
// crate's syscall-facing surface.
pub mod file;
mod ring;
mod setup;
mod worker;

pub use file::AsyncioFile;
pub use setup::{IoUringSetup, setup};

unittest::path_mod_test!(tests, "../ut-tests/context.rs");
