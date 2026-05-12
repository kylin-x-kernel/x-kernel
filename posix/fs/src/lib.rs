// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux filesystem compatibility helpers and syscall implementations.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod dir;
mod fd_ops;
mod io;
mod ioctl;
mod metadata;
mod mount;
mod namei;
mod open;
mod path;
mod pipe;
mod stat;
mod sync;

pub use self::{
    dir::*, fd_ops::*, io::*, ioctl::*, metadata::*, mount::*, namei::*, open::*, path::*, pipe::*,
    stat::*, sync::*,
};
