// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX/Linux ABI types.

#![no_std]

pub mod ipc;
pub mod ptr;
pub mod time;

pub use ipc::*;
pub use ptr::*;
pub use time::*;
