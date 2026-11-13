// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem syscall implementations.

#![no_std]

extern crate alloc;

#[macro_use]
extern crate klogger;

mod dir;
mod fd_ops;
pub mod file;
mod io;
mod ioctl;
mod metadata;
mod mount;
mod mount_api;
mod namei;
pub mod notify;
mod open;
mod path;
mod quota;
mod stat;
mod sync;
pub mod watch;
mod xattr;

pub use self::{
    dir::*, fd_ops::*, io::*, ioctl::*, metadata::*, mount::*, mount_api::*, namei::*, open::*,
    quota::*, stat::*, sync::*, xattr::*,
};
