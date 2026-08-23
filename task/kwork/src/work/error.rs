// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Errors returned by sleepable workerqueue APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkqueueError {
    /// The caller is in hardirq, serving-softirq, or BH-disabled context.
    InvalidContext,
    /// The current worker tried to wait on work that cannot make progress.
    ///
    /// This covers direct self-wait and worker callbacks waiting on pending or
    /// running work owned by the same execution pool. The running case is
    /// intentionally conservative because the target work may be requeued to
    /// the current bounded pool before reaching idle.
    SelfWait,
    /// The scheduler wait provider failed.
    WaitFailed,
    /// A delayed work flush could not queue the delayed instance.
    QueueFailed,
    /// The bounded barrier storage for the observed work instance is full.
    BarrierFull,
    /// The target workerqueue has no scheduler-owned worker able to drain it.
    WorkerUnavailable,
    /// The requested queue kind does not support this operation yet.
    UnsupportedQueue,
}

/// Result of a non-waiting cancel attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelWorkResult {
    /// A pending queued instance was removed.
    CancelledPending,
    /// No queued instance existed.
    NotPending,
    /// The callback is currently running.
    Running,
}
