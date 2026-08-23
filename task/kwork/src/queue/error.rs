// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Result of a non-blocking queue attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueWorkResult {
    /// The work was queued for execution.
    Queued,
    /// The work already has a queued instance.
    AlreadyQueued,
    /// The work is disabled or synchronous cancel is waiting for the callback.
    Disabled,
    /// The fixed queue has no free slot.
    QueueFull,
    /// The target logical CPU id is outside `NR_CPUS`.
    InvalidCpu,
    /// The target system worker pool is not ready to drain work.
    WorkerUnavailable,
}

/// Result of a delayed queue attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDelayedWorkResult {
    /// The work was queued immediately or armed for delayed queueing.
    Queued,
    /// The delayed work already has a delayed or queued instance.
    AlreadyQueued,
    /// The work is disabled or synchronous cancel is waiting for the callback.
    Disabled,
    /// The fixed target queue has no free slot.
    QueueFull,
    /// The target logical CPU id is outside `NR_CPUS`.
    InvalidCpu,
    /// The target system worker pool is not ready to drain work.
    WorkerUnavailable,
    /// Non-zero delayed queueing was requested from interrupt-like context.
    InvalidContext,
    /// The delay could not be represented as a monotonic deadline.
    TimerUnavailable,
}

impl From<QueueWorkResult> for QueueDelayedWorkResult {
    fn from(result: QueueWorkResult) -> Self {
        match result {
            QueueWorkResult::Queued => Self::Queued,
            QueueWorkResult::AlreadyQueued => Self::AlreadyQueued,
            QueueWorkResult::Disabled => Self::Disabled,
            QueueWorkResult::QueueFull => Self::QueueFull,
            QueueWorkResult::InvalidCpu => Self::InvalidCpu,
            QueueWorkResult::WorkerUnavailable => Self::WorkerUnavailable,
        }
    }
}

/// Errors returned when configuring a static logical workqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkQueueStartError {
    /// The caller is in hardirq, serving-softirq, or BH-disabled context.
    InvalidContext,
    /// Non-empty [`super::WorkQueueFlags`] were requested before that policy exists.
    UnsupportedFlags,
    /// The queue is one of the built-in system queues.
    SystemQueue,
}

/// Errors returned when allocating a dynamic workqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkQueueAllocError {
    /// The caller is in hardirq, serving-softirq, or BH-disabled context.
    InvalidContext,
    /// Non-empty [`super::WorkQueueFlags`] were requested before that policy exists.
    UnsupportedFlags,
}
