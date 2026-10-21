// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal task-reader, grace-period and retirement-callback bookkeeping.
//!
//! No ordinary consumer publication or reclamation API is exposed. The host
//! connects scheduler observations, interrupt watching and narrow idle/user windows;
//! the GP service samples those states without bypassing preempted readers.
//! A recorded scope alone is not permission to free or dereference retired data.
//! [`CallbackState`] grants one host [`CallbackDriver`] and unsafe callback
//! admission/execution; it does not start a task or publish runtime readiness.

#![no_std]

#[cfg(any(test, unittest))]
extern crate alloc;

// Exercise actual wrap in the same protocol during both host and kernel tests.
// This is private bookkeeping, never a public or persistent identity format.
#[cfg(any(test, unittest))]
type PeriodId = u8;
#[cfg(not(any(test, unittest)))]
type PeriodId = u64;

mod callback;

#[cfg_attr(
    not(any(test, unittest)),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: private callback protocol is exercised by deterministic \
                  tests before host service integration"
    )
)]
mod callback_queue;

#[cfg_attr(
    not(any(test, unittest)),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: private GP protocol is exercised by deterministic tests \
                  before host observation integration"
    )
)]
mod grace_period;
mod node;
mod reader;
mod service;

mod integration;
mod watching;

pub use callback::CallbackNode;
pub use callback_queue::CallbackDriver;
pub use integration::{RcuContextIf, ReaderCleanup, SchedulerObservation};
pub use node::ReaderNode;
pub use reader::{ReadGuard, ReaderCleanupOutcome, TaskReader};
pub use service::CallbackState;
pub use watching::{CpuWatching, InterruptEntry};
