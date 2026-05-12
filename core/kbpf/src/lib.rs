// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! eBPF execution engine for X-Kernel.
//!
//! This crate hosts a small, self-contained eBPF interpreter and the
//! supporting types (registers, decoded instructions, errors). It is
//! being built up incrementally.
//!
//! Whether this crate is part of the kernel build at all is decided by
//! the top-level `kfeat::ebpf` feature; there is no per-crate toggle
//! here — depending on `kbpf` always pulls in the full surface.
//!
//! Out of scope here (handled elsewhere or later): the BPF verifier,
//! the `bpf(2)` syscall surface, JIT, BPF maps, and the standard
//! Linux helper-function set.

#![cfg_attr(not(test), no_std)]

pub mod error;
pub mod insn;
pub mod vm;

pub use error::{Error, Result};
pub use insn::{Insn, SLOT_SIZE};
pub use vm::Vm;
