// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX credential syscall implementations.
//!
//! Linux keeps the UID/GID syscall surface mostly in `kernel/sys.c` and the
//! supplementary-group surface in `kernel/groups.c`. This crate mirrors that
//! split while keeping the pure credential model in `kcred`.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod groups;
mod helpers;
mod ids;

pub use self::{groups::*, ids::*};
