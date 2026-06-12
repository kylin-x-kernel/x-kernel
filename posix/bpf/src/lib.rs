// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `bpf(2)` — Linux-uapi-compatible surface (incremental).
//!
//! When feature `ebpf` is enabled, `sys_bpf` implements a subset of the
//! commands documented in Linux `include/uapi/linux/bpf.h`. Unsupported
//! commands return `kerrno::KError::OperationNotSupported` (`EOPNOTSUPP`).

#![no_std]

extern crate alloc;

#[cfg(feature = "ebpf")]
mod bpf;

#[cfg(feature = "ebpf")]
pub use bpf::sys_bpf;
