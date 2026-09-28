// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS adapter for the 9P2000.L client.
#![cfg_attr(any(not(test), doc), no_std)]
#![feature(likely_unlikely)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

mod dir;
mod file;
mod fs;
mod inode;
mod util;

pub use fs::mount;
