// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Submission authority without shared queue lifecycle authority.

use core::pin::Pin;

use kcpu_id_map::LogicalCpuId;
use ktime_types::TimeSpan;

use crate::{ArmError, DelayedWork, SubmitError, Work, services};

/// Selects a kernel-owned default service without exposing its queue handle.
///
/// Use only after kernel initialization. Each category covers the boot CPU set,
/// defaults to current-CPU routing and retains unfinished ownership. There is no
/// whole-queue flush or shutdown: stop/wait for your own Work instead. Callers
/// needing queue-wide control must use a separately owned queue.
///
/// ```compile_fail
/// kworkng::DefaultQueue::Normal.close_and_drain();
/// ```
/// ```compile_fail
/// kworkng::DefaultQueue::Normal.flush();
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DefaultQueue {
    /// Sleepable callbacks executed by normal worker tasks.
    Normal,
    /// Non-sleeping callbacks executed by the normal BH service.
    BottomHalf,
    /// Non-sleeping callbacks executed by the high-priority BH service.
    HighBottomHalf,
}

/// A default-service request was not accepted by this operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceError {
    /// Kernel initialization has not published the complete service set.
    NotReady,
    /// Ordinary work admission was rejected without accepting new work.
    Admission(SubmitError),
    /// Delayed admission or modification was rejected.
    Delayed(ArmError),
}

impl DefaultQueue {
    fn with_queue<R>(
        self,
        submit: impl FnOnce(&alloc::sync::Arc<kworkqueue_ng::service::Queue>) -> Result<R, ServiceError>,
    ) -> Result<R, ServiceError> {
        services::with_default_queue(self, |queue| {
            // Initialization reports its own failures. IRQ-capable producers
            // must not repeatedly enter the console on an unready service.
            let queue = queue.ok_or(ServiceError::NotReady)?;
            submit(queue)
        })
    }

    /// Submits prepared work without allocation, sleeping or inline execution.
    /// `None` selects current CPU or an unfinished owner; `Some(cpu)` targets
    /// explicitly. Valid in task/BH/hard IRQ context, not NMI.
    ///
    /// # Safety
    /// Retain pinned Work and its captures until producers/concurrent API calls
    /// are joined and cancel_sync succeeds. Pin is not ownership. The callback
    /// must obey this service's context, and its progress must not be obstructed.
    ///
    /// # Errors
    /// Returns NotReady without logging before admission if initialization is incomplete.
    /// Otherwise returns the ordinary admission rejection without accepting work.
    ///
    /// # Panics
    /// Propagates broken execution-provider or accounting invariants.
    pub unsafe fn queue_work<H: Fn() + Send + Sync>(
        self,
        work: Pin<&Work<H>>,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ServiceError> {
        self.with_queue(|queue| {
            // SAFETY: caller retains this pinned Work/captures through the specified
            // joins and drain; lookup supplies initialized kernel-lifetime providers.
            unsafe { queue.queue_work(work, target) }.map_err(ServiceError::Admission)
        })
    }

    /// Queues prepared delayed work; zero delay is ordinary queueing, not inline.
    /// CPU selection and execution context follow queue_work. No submission
    /// allocation or sleeping occurs; positive delays use monotonic time.
    ///
    /// # Safety
    /// Keep the pinned timer/Work pair and captures live through producer/API joins
    /// and successful DelayedWork::cancel_sync, even when logically idle. Obey the
    /// selected callback context and do not obstruct timer or callback progress.
    ///
    /// # Errors
    /// Returns NotReady without logging before touching the timer when uninitialized;
    /// otherwise reports delayed admission failure without accepting new work.
    ///
    /// # Panics
    /// Propagates broken timer binding, provider or accounting invariants.
    pub unsafe fn queue_delayed_work<H: Fn() + Send + Sync>(
        self,
        work: Pin<&DelayedWork<H>>,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ServiceError> {
        self.with_queue(|queue| {
            // SAFETY: the caller retains the complete pinned pair through both drains;
            // a ready default queue supplies the required execution providers.
            unsafe { queue.queue_delayed_work(work, delay, target) }.map_err(ServiceError::Delayed)
        })
    }

    /// Changes pending delay or accepts one occurrence if none is pending.
    /// Running work and unfinished ownership are preserved. Zero delay makes work
    /// ordinary without moving an already queued occurrence to the FIFO tail.
    /// Context and allocation behavior follow queue_delayed_work.
    ///
    /// # Safety
    /// Same storage, callback-context and final-drain obligations as
    /// queue_delayed_work; modification may publish previously idle storage.
    ///
    /// # Errors
    /// Returns NotReady without logging before timer access when uninitialized. Other
    /// failures leave the arrangement unchanged by this call; races may change it.
    ///
    /// # Panics
    /// Propagates broken timer binding, provider or accounting invariants.
    pub unsafe fn modify_delayed_work<H: Fn() + Send + Sync>(
        self,
        work: Pin<&DelayedWork<H>>,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ServiceError> {
        self.with_queue(|queue| {
            // SAFETY: caller retains the same pair through all fresh/replacement
            // publications and both drains; the service has completed initialization.
            unsafe { queue.modify_delayed_work(work, delay, target) }.map_err(ServiceError::Delayed)
        })
    }
}
