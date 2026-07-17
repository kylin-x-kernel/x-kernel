// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible futex keys, waiters, and ordering buckets.

#![no_std]

extern crate alloc;

mod key;
mod table;
mod waiter;
mod wake_op;

pub use self::{key::FutexKey, table::global_table, wake_op::FutexWakeOp};
