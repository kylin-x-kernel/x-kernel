// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux cgroup v2 filesystem adapter.
//!
//! The crate translates VFS operations into the canonical cgroup and process
//! ownership APIs. It does not own hierarchy or task-membership state.

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

mod command;
mod control;
mod controller;
mod dir;
mod mount;
mod pids;
mod process;
mod state;

pub use mount::{FILE_SYSTEM_TYPE, new_cgroup2fs, new_initial_cgroup2fs};

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
