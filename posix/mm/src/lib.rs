// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX memory management syscall implementations.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod brk;
mod memfd;
mod mincore;
mod mmap;

pub use self::{brk::*, memfd::*, mincore::*, mmap::*};
