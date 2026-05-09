// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX I/O multiplexing syscall implementations.
//!
//! This crate currently owns `epoll` and can later absorb the remaining
//! POSIX/Linux I/O multiplexing entry points such as `poll` and `select`.

#![no_std]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod epoll;

pub use self::epoll::*;
