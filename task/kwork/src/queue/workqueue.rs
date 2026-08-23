// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kcpu_id_map::LogicalCpuId;
use klazy::Once;
#[cfg(unittest)]
use kpoll::PollEvent;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;

use super::{
    QueueColorFlush, QueueDelayedWorkResult, QueueInstanceCompletion, QueueOwner, QueueWake,
    QueueWorkResult, WorkQueueAllocError, WorkQueueAttrs, WorkQueueStartError, WorkQueueSyncState,
    prepare_queue_color_flush, validate_workqueue_attrs, wait_for_queue_color_flush,
};
use crate::{
    DelayedScheduledWork, DelayedWorkTarget, ScheduledWork, TaskPoolBinding, WorkColor,
    WorkQueuePoolState, WorkQueueRuntime, WorkqueueContextIf, WorkqueueError, builtin_queue_cpu,
    finish_workqueue_pool_enqueue, mod_delayed_work_for_target, queue_delayed_work_for_target,
    queue_result_to_wait_error, reject_invalid_wait_context, reject_worker_pool_wait_deadlock,
    wait_for_workqueue_idle,
};
pub struct WorkQueue {
    name: &'static str,
    pub(crate) state: SpinNoIrq<WorkQueueState>,
    pool_states: [SpinNoIrq<WorkQueuePoolState>; kbuild_config::NR_CPUS],
    sync: Once<WorkQueueSyncState>,
}

impl WorkQueue {
    /// Creates an empty fixed workerqueue.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            state: SpinNoIrq::new(WorkQueueState::new()),
            pool_states: [const { SpinNoIrq::new(WorkQueuePoolState::new()) };
                kbuild_config::NR_CPUS],
            sync: Once::new(),
        }
    }

    /// Returns the queue name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn key(&self) -> usize {
        core::ptr::from_ref(self).addr()
    }

    pub(crate) fn pool_state_for_cpu(
        &self,
        cpu_id: LogicalCpuId,
    ) -> Option<&SpinNoIrq<WorkQueuePoolState>> {
        self.pool_states.get(cpu_id.as_usize())
    }

    pub(crate) fn sync(&self) -> &WorkQueueSyncState {
        self.sync.call_once(WorkQueueSyncState::new)
    }

    pub(crate) fn sync_if_initialized(&self) -> Option<&WorkQueueSyncState> {
        self.sync.get()
    }

    pub(crate) fn collect_waiters(&self, completion: QueueInstanceCompletion) -> QueueWake {
        let mut idle = None;
        let mut flush = None;
        if let Some(sync) = self.sync_if_initialized() {
            if completion.is_idle() {
                idle = Some(sync.idle_completion().complete_all_defer_wake());
            }
            let queue_flush_completed = completion.drained_color().is_some_and(|color| {
                self.state
                    .lock()
                    .complete_queue_flush_color_if_active(color)
            });
            if completion.flush_completed() || queue_flush_completed {
                flush = Some(sync.flush_event().notify_defer_wake());
            }
        }
        QueueWake::new(idle, flush)
    }

    pub(crate) fn reinit_idle_waiters_if_initialized(&self) {
        if let Some(sync) = self.sync_if_initialized() {
            sync.idle_completion().reinit();
        }
    }

    pub(crate) fn is_destroying(&self) -> bool {
        self.state.lock().is_destroying
    }

    pub(crate) fn reject_queue_if_destroying(&self) -> Result<(), QueueWorkResult> {
        if self.is_destroying() {
            Err(QueueWorkResult::Disabled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.pool_states.iter().all(|state| state.lock().is_idle())
    }

    fn configure_max_active(&self, max_active: usize) {
        for cpu_index in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu_index);
            let Some(binding) = TaskPoolBinding::default_for_cpu(cpu_id) else {
                continue;
            };
            let pool = binding.pool();
            let mut pool_state = pool.state.lock();
            let mut binding_state = self.pool_states[cpu_index].lock();
            pool_state.configure_binding_max_active(&mut binding_state, self.key(), max_active);
            let wake_plan = pool_state.select_worker_to_kick();
            drop(binding_state);
            drop(pool_state);
            binding.wake(wake_plan).execute();
        }
    }

    #[cfg(unittest)]
    pub(crate) fn pending_len_for_tests(&self) -> usize {
        let cpu_id = crate::WorkqueueHostIf::current_cpu_id();
        let Some(binding) = TaskPoolBinding::default_for_cpu(cpu_id) else {
            return 0;
        };
        binding
            .pool()
            .state
            .lock()
            .pending_len_for_binding(self.key())
    }

    #[cfg(unittest)]
    pub(crate) fn has_runnable_work_for_tests(&self) -> bool {
        let cpu_id = crate::WorkqueueHostIf::current_cpu_id();
        let Some(binding) = TaskPoolBinding::default_for_cpu(cpu_id) else {
            return false;
        };
        binding
            .pool()
            .state
            .lock()
            .runnable_len_for_binding(self.key())
            != 0
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_tests(&self) -> usize {
        let cpu_id = crate::WorkqueueHostIf::current_cpu_id();
        let Some(binding) = TaskPoolBinding::default_for_cpu(cpu_id) else {
            return 0;
        };
        binding
            .pool()
            .state
            .lock()
            .runnable_len_for_binding(self.key())
    }

    #[cfg(unittest)]
    pub(crate) fn active_len_for_tests(&self) -> usize {
        self.pool_state_for_cpu(crate::WorkqueueHostIf::current_cpu_id())
            .map_or(0, |state| state.lock().active_count_for_tests())
    }

    #[cfg(unittest)]
    pub(crate) fn configure_max_active_for_tests(&self, max_active: usize) {
        self.configure_max_active(max_active);
    }

    /// Configures this static logical workqueue.
    ///
    /// Work submitted to the queue is executed by the shared per-CPU worker
    /// pool. This call configures queue-local policy and does not create worker
    /// tasks. The queue object must remain valid while submitted work can still
    /// reference it.
    ///
    /// # Errors
    ///
    /// Returns [`WorkQueueStartError::SystemQueue`] for built-in system
    /// queues, [`WorkQueueStartError::UnsupportedFlags`] for non-empty
    /// Linux-like policy flags, and
    /// [`WorkQueueStartError::InvalidContext`] when called from interrupt-like
    /// context.
    pub fn start(&'static self, attrs: WorkQueueAttrs) -> Result<(), WorkQueueStartError> {
        if WorkqueueContextIf::is_invalid_wait_context() {
            return Err(WorkQueueStartError::InvalidContext);
        }
        if builtin_queue_cpu(self).is_some() {
            return Err(WorkQueueStartError::SystemQueue);
        }
        let config = validate_workqueue_attrs(attrs)?;
        self.configure_max_active(config.max_active);
        Ok(())
    }

    /// Queues work on this fixed workerqueue without blocking.
    ///
    /// This function may be called from hardirq, serving-softirq, BH-disabled,
    /// or task context. It does not allocate and does not execute the
    /// callback. All task-context queues attach to the shared per-CPU worker
    /// pool.
    ///
    /// Returns [`QueueWorkResult::Queued`] on success,
    /// [`QueueWorkResult::AlreadyQueued`] when the work already has a queued
    /// instance, [`QueueWorkResult::QueueFull`] when the fixed entry ring is
    /// full, [`QueueWorkResult::Disabled`] for disabled work,
    /// [`QueueWorkResult::InvalidCpu`] for out-of-range CPU targets, and
    /// [`QueueWorkResult::WorkerUnavailable`] when the matching system worker
    /// pool has not been installed yet.
    pub fn queue_work(&'static self, work: &ScheduledWork) -> QueueWorkResult {
        match self.select_pool_binding(None) {
            Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
            Err(result) => result,
        }
    }

    /// Queues delayed work on this fixed workerqueue.
    ///
    /// A zero delay is equivalent to [`Self::queue_work`]. A non-zero delay
    /// arms a timer and queues the inner work when the timer expires.
    pub fn queue_delayed_work(
        &'static self,
        work: &DelayedScheduledWork,
        delay: TimeSpan,
    ) -> QueueDelayedWorkResult {
        queue_delayed_work_for_target(DelayedWorkTarget::Static(self), work, delay)
    }

    /// Modifies delay of a delayed work on this fixed workerqueue.
    ///
    /// This supports modifying timer-reserved delayed work in place. If the
    /// embedded work already reached a normal worklist, this returns
    /// [`QueueDelayedWorkResult::AlreadyQueued`] instead of stealing it from
    /// the queue.
    pub fn mod_delayed_work(
        &'static self,
        work: &DelayedScheduledWork,
        delay: TimeSpan,
    ) -> QueueDelayedWorkResult {
        mod_delayed_work_for_target(DelayedWorkTarget::Static(self), work, delay)
    }

    /// Waits until work queued before this static workqueue flush has
    /// finished.
    ///
    /// Built-in system queues and started custom queues use the same
    /// `WorkQueuePoolBinding` color accounting; work queued after the flush captures
    /// its color does not extend the wait.
    ///
    /// # Errors
    ///
    /// Returns [`WorkqueueError::InvalidContext`] in interrupt-like context,
    /// [`WorkqueueError::SelfWait`] when the current worker callback flushes
    /// its own execution pool, and
    /// [`WorkqueueError::WaitFailed`] when the wait provider fails.
    pub fn flush(&'static self) -> Result<(), WorkqueueError> {
        reject_invalid_wait_context()?;
        flush_owner(QueueOwner::Static(self))
    }
}

/// A refcounted dynamic workqueue handle.
///
/// Dynamic queues attach to shared per-CPU worker pools during [`Self::alloc`].
/// Queueing through this handle is IRQ-safe; lifecycle operations such as
/// [`Self::flush`] and [`Self::destroy`] may sleep.
#[derive(Clone)]
pub struct WorkQueueHandle {
    inner: Arc<WorkQueue>,
}

impl WorkQueueHandle {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            inner: Arc::new(WorkQueue::new(name)),
        }
    }

    /// Allocates a dynamic logical workqueue.
    ///
    /// The queue uses shared per-CPU worker pools. Subsequent
    /// [`Self::queue_work`] calls are IRQ-safe and never allocate worker tasks.
    ///
    /// # Errors
    ///
    /// Returns [`WorkQueueAllocError::InvalidContext`] when called from an
    /// interrupt-like context and [`WorkQueueAllocError::UnsupportedFlags`] for
    /// non-empty Linux-like policy flags.
    pub fn alloc(name: &'static str, attrs: WorkQueueAttrs) -> Result<Self, WorkQueueAllocError> {
        if WorkqueueContextIf::is_invalid_wait_context() {
            return Err(WorkQueueAllocError::InvalidContext);
        }
        let config = validate_workqueue_attrs(attrs)?;

        let queue = Self::new(name);
        queue.queue().configure_max_active(config.max_active);
        Ok(queue)
    }

    pub(crate) fn queue(&self) -> &WorkQueue {
        self.inner.as_ref()
    }

    #[cfg(unittest)]
    pub(crate) fn has_runnable_work_for_tests(&self) -> bool {
        self.queue().has_runnable_work_for_tests()
    }

    #[cfg(unittest)]
    pub(crate) fn runnable_len_for_tests(&self) -> usize {
        self.queue().runnable_len_for_tests()
    }

    #[cfg(unittest)]
    pub(crate) fn active_len_for_tests(&self) -> usize {
        self.queue().active_len_for_tests()
    }

    #[cfg(unittest)]
    pub(crate) fn configure_max_active_for_tests(&self, max_active: usize) {
        self.queue().configure_max_active_for_tests(max_active);
    }

    #[cfg(unittest)]
    pub(crate) fn flush_event(&self) -> &PollEvent {
        self.queue().sync().flush_event()
    }

    /// Returns whether two handles refer to the same dynamic queue.
    pub fn same_queue(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns the queue name.
    pub fn name(&self) -> &'static str {
        self.queue().name()
    }

    /// Queues work on this dynamic workqueue without blocking.
    ///
    /// This function may be called from hardirq, serving-softirq, BH-disabled,
    /// or task context.
    ///
    /// Returns [`QueueWorkResult::Queued`] on success,
    /// [`QueueWorkResult::AlreadyQueued`] when the work already has a queued
    /// instance, [`QueueWorkResult::QueueFull`] when the fixed entry ring is
    /// full, and [`QueueWorkResult::Disabled`] for disabled work or queues
    /// that are being destroyed.
    pub fn queue_work(&self, work: &ScheduledWork) -> QueueWorkResult {
        match self.clone().select_pool_binding(None) {
            Ok(binding) => finish_workqueue_pool_enqueue(binding.queue_work(work)),
            Err(result) => result,
        }
    }

    /// Queues delayed work on this dynamic workqueue.
    ///
    /// A zero delay is equivalent to [`Self::queue_work`]. A non-zero delay
    /// arms a timer and queues the inner work when the timer expires.
    pub fn queue_delayed_work(
        &self,
        work: &DelayedScheduledWork,
        delay: TimeSpan,
    ) -> QueueDelayedWorkResult {
        queue_delayed_work_for_target(DelayedWorkTarget::Dynamic(self.clone()), work, delay)
    }

    /// Modifies delay of delayed work on this dynamic workqueue.
    pub fn mod_delayed_work(
        &self,
        work: &DelayedScheduledWork,
        delay: TimeSpan,
    ) -> QueueDelayedWorkResult {
        mod_delayed_work_for_target(DelayedWorkTarget::Dynamic(self.clone()), work, delay)
    }

    /// Waits until work queued before this dynamic workqueue flush has
    /// finished.
    ///
    /// This is a Linux-like color flush: work queued after this call captures
    /// its flush color does not extend the wait. Concurrent flushers on the
    /// same queue reserve distinct colors while the color space has room; if
    /// all colors are still in flight, a flusher waits for one color to drain
    /// before retrying. Use [`Self::destroy`] for teardown after producers
    /// have been stopped.
    ///
    /// # Errors
    ///
    /// Returns [`WorkqueueError::InvalidContext`] in interrupt-like context,
    /// [`WorkqueueError::SelfWait`] when the current worker callback flushes
    /// its own queue, and [`WorkqueueError::WaitFailed`] when the wait
    /// provider fails.
    pub fn flush(&self) -> Result<(), WorkqueueError> {
        reject_invalid_wait_context()?;
        flush_owner(QueueOwner::Dynamic(self.clone()))
    }

    /// Destroys this dynamic workqueue after draining its pending and running
    /// work.
    ///
    /// This function first gates new enqueue attempts and then waits for all
    /// pending and running work to drain from the shared pools. Calls to
    /// [`Self::queue_work`] after the destroy gate return
    /// [`QueueWorkResult::Disabled`].
    ///
    /// If waiting fails, the caller retains the handle and may retry. The
    /// queue remains in the destroying state so new enqueue attempts stay
    /// rejected.
    ///
    /// # Errors
    ///
    /// Returns [`WorkqueueError::InvalidContext`] in interrupt-like context,
    /// [`WorkqueueError::SelfWait`] when the current worker callback destroys
    /// its own queue, and [`WorkqueueError::WaitFailed`] when the idle wait
    /// provider fails.
    pub fn destroy(&self) -> Result<(), WorkqueueError> {
        reject_invalid_wait_context()?;
        let binding = self
            .clone()
            .select_pool_binding(None)
            .map_err(queue_result_to_wait_error)?;
        reject_worker_pool_wait_deadlock(binding.pool_key())?;

        self.queue().state.lock().is_destroying = true;
        wait_for_workqueue_idle(self)?;
        Ok(())
    }
}

fn flush_owner(owner: QueueOwner) -> Result<(), WorkqueueError> {
    let bindings = match owner {
        QueueOwner::Static(queue) => queue.all_pool_bindings(),
        QueueOwner::Dynamic(queue) => queue.all_pool_bindings(),
    }
    .map_err(queue_result_to_wait_error)?;

    // Reject every dependency before advancing any color. Returning halfway
    // through a multi-binding flush would leave an observable partial snapshot.
    for binding in &bindings {
        reject_worker_pool_wait_deadlock(binding.pool_key())?;
    }

    loop {
        match prepare_queue_color_flush(&bindings) {
            QueueColorFlush::Done => return Ok(()),
            QueueColorFlush::Wait(color) => wait_for_queue_color_flush(&bindings, color)?,
            QueueColorFlush::Overflow(color) => wait_for_queue_color_flush(&bindings, color)?,
        }
    }
}

pub(crate) struct WorkQueueState {
    pub(crate) is_destroying: bool,
    active_flush_colors: [bool; WorkColor::COUNT],
}

impl WorkQueueState {
    pub(crate) const fn new() -> Self {
        Self {
            is_destroying: false,
            active_flush_colors: [false; WorkColor::COUNT],
        }
    }

    pub(super) fn arm_queue_flush_color(&mut self, color: WorkColor) {
        self.active_flush_colors[color.index()] = true;
    }

    fn complete_queue_flush_color_if_active(&mut self, color: WorkColor) -> bool {
        let active = &mut self.active_flush_colors[color.index()];
        if !*active {
            return false;
        }
        *active = false;
        true
    }
}
