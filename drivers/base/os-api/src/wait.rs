// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-provided sleeping for drivers that wait on a device completion.

use alloc::sync::Arc;

use kcompletion::SyncWake;

use crate::ResError;

/// Host capability for parking the calling task until a driver's completion.
///
/// A driver that hands work to a device and is woken by its interrupt uses
/// this instead of polling. The host owns parking and signal policy; the
/// driver owns the predicate and the state it reads.
pub trait WaitOp: Send + Sync {
    /// Parks the calling task until `wake` is notified.
    ///
    /// The driver notifies `wake` after publishing the state its waiter
    /// reads, so a notification that races the park is still observed. The
    /// call returns once that notification (or a task interrupt) has ended the
    /// sleep; the caller rechecks its own state then.
    ///
    /// # Errors
    ///
    /// Returns [`ResError::Busy`] when no task can be parked in the current
    /// context.
    fn sleep(&self, wake: Arc<dyn SyncWake + Send>) -> Result<(), ResError>;
}
