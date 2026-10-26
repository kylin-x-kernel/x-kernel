// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned blocking mutexes independent of a concrete scheduler or HAL.
//!
//! [`Mutex`] and [`RawMutex`] retain the adaptive spinning and event-listener
//! algorithm used by `ksync`. The host supplies [`kiface_task_api::TaskExecutionIf`]
//! for identity, yielding and waiting, and [`kiface_task_api::TaskLockDiagnosticsIf`]
//! for watchdog diagnostics. The task-domain contracts live in `kiface-task-api`.
//! Blocking acquisition requires sleepable task context and is not recursive.
//! Guards are not Send, but the owning task may migrate while holding one.
//!
//! `stats` records acquisitions and blocking contention by lock class;
//! `watchdog` enables calls to the always-declared diagnostic hooks.
//!
//! ```no_run
//! // After installing the task provider, in sleepable task context:
//! let value = kmutex::Mutex::new(0);
//! *value.lock() += 1;
//! ```

#![no_std]

mod mutex;
mod spin;

pub use mutex::{Mutex, MutexGuard, RawMutex};
pub use spin::SpinConfig;
