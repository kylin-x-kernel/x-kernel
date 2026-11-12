// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible futex keys, waiters, and ordering buckets.
//!
//! Entry point is [`global_table()`], returning the zero-sized
//! `GlobalFutexTable` handle with the `wait`/`wake`/`wake_op`/`requeue`
//! operations. Keys are
//! built from [`FutexKey::resolve`] (VMA-aware) or
//! [`FutexKey::resolve_private`] (`FUTEX_PRIVATE_FLAG` fast path), and
//! `FUTEX_WAKE_OP` operands decode through [`FutexWakeOp::decode`].
//! `lock_pi` / `trylock_pi` / `unlock_pi` expose kernel PI ownership from a
//! key and validated task references. User variants synchronize the lock word
//! through process services provided by [`PiFutexContext`].
//!
//! The crate owns only the concurrency core: syscall ABI decoding, count
//! clamping, timeouts, and robust-list processing belong to `ksyscall` and
//! the posix process layer. See `docs/design.md` for the bucket, waiter,
//! and lock-ordering model.
#![no_std]

extern crate alloc;

mod key;
mod table;
mod waiter;
mod wake_op;

pub use self::{
    key::FutexKey,
    table::{PiFutexContext, global_table},
    wake_op::FutexWakeOp,
};
