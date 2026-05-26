// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TTY subsystem and terminal line discipline.

#![no_std]
#![feature(bstr)]

#[macro_use]
extern crate klogger;

extern crate alloc;

pub mod terminal;
pub mod tty;
