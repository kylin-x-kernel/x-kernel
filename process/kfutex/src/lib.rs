// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-side futex state and wait-queue ownership.

#![no_std]

extern crate alloc;

mod key;
mod process_state;
mod table;
mod wait_queue;

pub use self::{
    key::FutexKey,
    process_state::ProcessFutexState,
    table::{FutexEntry, FutexGuard, FutexTable},
    wait_queue::WaitQueue,
};
