// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `bpf(2)` — Linux-uapi-compatible surface (incremental).
//!
//! `sys_bpf` implements a subset of the commands documented in Linux
//! `include/uapi/linux/bpf.h`, including the cgroup-v2 device controller
//! surface (`BPF_PROG_TYPE_CGROUP_DEVICE` attach/detach/link/query). The
//! programs run on the `kbpf` interpreter and are enforced at the device
//! open/mknod paths through the `bpffs` device controller. Unsupported
//! commands return `kerrno::KError::OperationNotSupported` (`EOPNOTSUPP`).

#![no_std]

extern crate alloc;

mod bpf;

pub use bpf::sys_bpf;
