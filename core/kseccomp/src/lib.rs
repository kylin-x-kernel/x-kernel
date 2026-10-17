// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! seccomp classic-BPF filter engine for X-Kernel.
//!
//! This crate hosts the classic-BPF interpreter used by the `seccomp(2)`
//! syscall, the `struct seccomp_data` snapshot model, and the
//! [`Decision`] type that maps a filter's return value onto a concrete
//! syscall-entry action (allow, errno, kill-process, kill-thread, ...).
//!
//! The interpreter is a self-contained, `no_std`, allocation-only engine:
//! it evaluates a copied [`SockFilter`] program against a
//! [`SeccompData`] snapshot and returns the raw `SECCOMP_RET_*` value.
//! Decision precedence across a chained filter list is resolved by
//! [`Action::precedence`] following Linux `seccomp_run_filters`.
//!
//! The crate deliberately knows nothing about thread/process state,
//! syscall dispatch, or user memory. `ksyscall` reads the user-supplied
//! `sock_fprog`, builds a [`SeccompFilter`], stores it on the thread, and
//! evaluates it at syscall entry before dispatch.
//!
//! # Execution context
//!
//! Pure computation: no scheduler, CPU-local state, or device access.
//! Callers must hold the filter program `Arc` that owns the instructions.
//!
//! # Example
//!
//! ```
//! use kseccomp::{Action, Decision, SeccompData, SockFilter};
//!
//! // `BPF_RET | BPF_K` with `SECCOMP_RET_ALLOW`: allow every syscall.
//! let program = [SockFilter {
//!     code: 0x06,
//!     jt: 0,
//!     jf: 0,
//!     k: 0x7fff_0000,
//! }];
//! let data = SeccompData::new(0, kseccomp::AUDIT_ARCH_X86_64, 0, [0; 6]);
//! let raw = kseccomp::run(&program, data.as_bytes());
//! assert_eq!(Decision::from(Action::from_ret(raw)), Decision::Allow);
//! ```

#![no_std]
#![warn(missing_docs)]
// The interpreter is pure arithmetic over a copied program and a byte
// snapshot, so it needs no `unsafe` at all.
#![forbid(unsafe_code)]

extern crate alloc;

mod bpf;
mod data;
mod decision;
mod filter;
mod opcode;

pub use bpf::run;
pub use data::{
    ARCH_OFFSET, ARGS_OFFSET, AUDIT_ARCH_AARCH64, AUDIT_ARCH_LOONGARCH64, AUDIT_ARCH_RISCV64,
    AUDIT_ARCH_X86_64, IP_OFFSET, NR_OFFSET, SeccompData,
};
pub use decision::{Action, Decision, highest_priority};
pub use filter::{SeccompFilter, SeccompFilterError};

/// A single classic-BPF instruction as read from user space.
pub type SockFilter = linux_raw_sys::ptrace::sock_filter;

/// Size in bytes of the `struct seccomp_data` snapshot.
pub const SECCOMP_DATA_SIZE: usize = 64;

/// Maximum number of instructions per seccomp filter (`BPF_MAXINSNS`).
pub const BPF_MAXINSNS: usize = 4096;

unittest::path_mod_test!(tests_bpf, "../ut-tests/bpf.rs");
unittest::path_mod_test!(tests_data, "../ut-tests/data.rs");
unittest::path_mod_test!(tests_decision, "../ut-tests/decision.rs");
unittest::path_mod_test!(tests_filter, "../ut-tests/filter.rs");
