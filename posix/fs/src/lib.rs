// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux filesystem compatibility helpers and syscall implementations.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod ctl;
mod fd_ops;
mod io;
mod mount;
mod open;
mod path;
mod pipe;
mod stat;

pub use self::{ctl::*, fd_ops::*, io::*, mount::*, open::*, path::*, pipe::*, stat::*};
