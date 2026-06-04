// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FD-backed kernel object implementations.
//!
//! This crate owns kernel objects that are exposed through the process fd table
//! but are not fundamentally VFS path objects. Syscall ABI adapters live in
//! `ksyscall`; this crate owns the object state, invariants, and `FileLike` /
//! `Pollable` behavior.

#![no_std]

extern crate alloc;

pub mod eventfd;
pub mod timerfd;
