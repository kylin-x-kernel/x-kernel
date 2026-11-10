// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-provided sleeping for drivers that wait on a device completion.

use kcompletion::WaitQueue;

use crate::ResError;

/// Host capability for parking the calling task until a driver's completion.
///
/// A driver that hands work to a device and is woken by its interrupt uses
/// this instead of polling. The host owns parking and signal policy; the
/// driver owns the predicate and the state it reads.
pub trait WaitOp: Send + Sync {
    /// Parks the calling task until `queue` is notified.
    ///
    /// The driver owns `queue` and calls `notify_all` after publishing the
    /// state its waiter reads. The call returns once that notification (or a
    /// task interrupt) has ended the sleep; the caller rechecks its own state.
    ///
    /// # Errors
    ///
    /// Returns [`ResError::Busy`] when no task can be parked in the current
    /// context, and [`ResError::Interrupted`] when a fatal signal ends the
    /// park.
    fn sleep(&self, queue: &WaitQueue) -> Result<(), ResError>;
}
