// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

mod cow;
mod file;
pub mod mmap;

pub use self::{
    cow::{CowBackend, new_alloc, new_cow},
    file::{FileBackend, new_file},
    mmap::FileMapper,
};
