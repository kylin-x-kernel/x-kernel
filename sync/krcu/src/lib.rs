// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel RCU state, execution-context integration and progress service.
//!
//! The algorithm lives in `krcu-core`; this crate owns its system instances,
//! task/timer/softirq execution and deferred-cleanup policy. Scheduler and
//! interrupt entry points notify it through the statically linked integration
//! contract. Ordinary consumer read and retirement admission remains closed.

#![no_std]

extern crate alloc;

mod context;
#[cfg(unittest)]
mod read;
mod service;
#[cfg(unittest)]
#[path = "../ut-tests/support.rs"]
mod test_support;

pub use context::{enter_user_quiescent, exit_user_quiescent};
pub use service::init_service;
