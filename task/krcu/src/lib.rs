// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal task-reader, grace-period and retirement-callback bookkeeping.
//!
//! No public publication or reclamation API is exposed. The private node and
//! coordinator/callback protocol has no hardware quiescent-observation integration.
//! A recorded scope alone is not permission to free or dereference retired data.

#![no_std]

#[cfg(any(test, unittest))]
extern crate alloc;

// Exercise actual wrap in the same protocol during both host and kernel tests.
// This is private bookkeeping, never a public or persistent identity format.
#[cfg(any(test, unittest))]
type PeriodId = u8;
#[cfg(not(any(test, unittest)))]
type PeriodId = u64;

#[cfg_attr(
    not(any(test, unittest)),
    expect(
        dead_code,
        reason = "CG-024-lint-exceptions: private callback storage is exercised by deterministic \
                  tests before host service integration"
    )
)]
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

pub use node::ReaderNode;
pub use reader::{ReadGuard, TaskReader};
