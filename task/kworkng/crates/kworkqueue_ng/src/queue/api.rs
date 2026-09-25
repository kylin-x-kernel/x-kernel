// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Borrowed publication boundary; routing providers remain internal.

use alloc::sync::Arc;
use core::pin::Pin;

use kcpu_id_map::LogicalCpuId;
use ktime_types::TimeSpan;

use super::Queue;
use crate::{ArmError, DelayedWork, SubmitError, Work};

impl Queue {
    /// Queues prepared ordinary work without allocation, sleeping or inline execution.
    ///
    /// `target = None` reuses unfinished ownership or applies the queue's CPU policy;
    /// `Some(cpu)` requests that CPU and rejects conflicting unfinished ownership.
    /// Success returns the actual execution CPU. Task, BH and hard IRQ are supported,
    /// not NMI. The queue's execution providers must already be initialized.
    ///
    /// # Safety
    /// Keep Work and captures pinned, live and shared until all asynchronous accesses
    /// end. Pin alone does not retain storage. Stop/join producers and concurrent API
    /// calls, then successfully cancel_sync before freeing it. The callback must
    /// obey the queue category (BH callbacks cannot sleep), and waits must not block
    /// its progress. The queue never owns Work or its captures.
    ///
    /// # Errors
    /// Returns a distinguishable admission rejection without accepting new work.
    /// Concurrent rejection reasons have no guaranteed priority.
    ///
    /// # Panics
    /// Propagates broken accounting or execution-provider notification invariants.
    pub unsafe fn queue_work<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: Pin<&Work<H>>,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, SubmitError> {
        // SAFETY: the caller retains this exact pinned Work through producer join
        // and completion; routing samples the actual CPU using the kernel provider.
        unsafe { self.submit(work.get_ref(), target, khal::percpu::this_cpu_id) }
    }

    /// Queues prepared delayed work using a relative monotonic delay.
    ///
    /// CPU selection follows queue_work. Zero delay queues immediately, never inline;
    /// positive delay does not become eligible early. No submission allocation or
    /// sleep occurs. Requires initialized timer and execution providers; valid in
    /// task, BH and hard IRQ context, not NMI.
    ///
    /// # Safety
    /// Retain pinned DelayedWork and captures through producer/API joins and a
    /// successful DelayedWork::cancel_sync, including logically idle objects whose
    /// timer delivery may still be finishing. Queue shutdown does not drain timer-only
    /// work. Callback context and progress obligations match queue_work.
    ///
    /// # Errors
    /// Rejects invalid deadlines or admission without changing an existing occurrence.
    ///
    /// # Panics
    /// Propagates timer binding, accounting and notification invariant failures.
    pub unsafe fn queue_delayed_work<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: Pin<&DelayedWork<H>>,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        // SAFETY: this is the caller's retained pinned timer/Work pair, with both
        // initialized providers and the declared final ordinary/physical drain.
        unsafe { work.queue_after(self, target, delay, khal::percpu::this_cpu_id) }
    }

    /// Replaces a pending delay, or queues one occurrence when none is pending.
    ///
    /// Running work is not changed. Unfinished Queue/CPU ownership is preserved;
    /// zero delay makes pending work ordinary without moving an already queued
    /// occurrence to the FIFO tail. Context and allocation behavior match
    /// queue_delayed_work. Success returns the selected execution CPU.
    ///
    /// # Safety
    /// Same pinned storage, callback context, initialized providers and final drain
    /// obligations as queue_delayed_work. This call can publish fresh work even
    /// when no earlier occurrence exists.
    ///
    /// # Errors
    /// Invalid deadlines, ownership or admission leave the arrangement unchanged
    /// by this call; independent concurrent operations may still take effect.
    ///
    /// # Panics
    /// Propagates timer binding, accounting and notification invariant failures.
    pub unsafe fn modify_delayed_work<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: Pin<&DelayedWork<H>>,
        delay: TimeSpan,
        target: Option<LogicalCpuId>,
    ) -> Result<LogicalCpuId, ArmError> {
        // SAFETY: caller retains the same pinned timer/Work pair through all
        // publications and drains; the CPU sampler is the actual kernel provider.
        unsafe { work.modify_after(self, target, delay, khal::percpu::this_cpu_id) }
    }
}
