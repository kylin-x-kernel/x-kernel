// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS bridge for the 9P client library.
#![cfg_attr(any(not(test), doc), no_std)]
#![feature(likely_unlikely)]
#![allow(clippy::new_ret_no_self)]

extern crate alloc;

mod fs;
mod inode;
mod util;

pub use fs::Fs9pFilesystem;
pub use fs9p::Transport;
