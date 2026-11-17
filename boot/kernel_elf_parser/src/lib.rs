// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]
#![doc = include_str!("../README.md")]

extern crate alloc;

mod auxv;
mod error;
mod info;
mod user_stack;

pub use self::{
    auxv::*,
    error::ElfError,
    info::*,
    user_stack::{app_stack_region, app_stack_region_with_auxv},
};
