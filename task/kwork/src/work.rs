// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Callback-facing workqueue objects.

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use bitflags::bitflags;
use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktime_types::TimeSpan;

use crate::{
    builtinpool::{self, SystemPoolBinding, SystemPoolKind},
    builtinwq::{
        self, BottomHalfWorkQueueKind, SystemWorkQueueKind, system_bh_highpri_wq, system_bh_wq,
        system_percpu_wq, system_wq,
    },
    runtime::{self, WorkqueueTimerHandle},
};

pub const DEFAULT_WORKQUEUE_MAX_ACTIVE: usize = 256;
pub const MAX_WORKQUEUE_PENDING: usize = kbuild_config::WORKQUEUE_PENDING_CAP;

type CoreQueue =
    kworkqueue::WorkQueue<{ kbuild_config::NR_CPUS }, { kbuild_config::WORKQUEUE_PENDING_CAP }>;
pub(crate) type CoreQueueBinding<'queue> = kworkqueue::WorkQueueBinding<
    'queue,
    { kbuild_config::NR_CPUS },
    { kbuild_config::WORKQUEUE_PENDING_CAP },
>;

static SCHEDULED_WORK_REGISTRY: SpinNoIrq<Vec<Weak<ScheduledWorkInner>>> =
    SpinNoIrq::new(Vec::new());
static ACTIVE_SCHEDULED_WORKS: SpinNoIrq<Vec<Arc<ScheduledWorkInner>>> = SpinNoIrq::new(Vec::new());
static WORKQUEUE_REGISTRY: SpinNoIrq<Vec<RegisteredWorkQueue>> = SpinNoIrq::new(Vec::new());
static BINDING_REGISTRY: SpinNoIrq<BTreeMap<kworkqueue::BindingId, RegisteredBinding>> =
    SpinNoIrq::new(BTreeMap::new());

/// Result of an enqueue attempt through the callback API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueWorkResult {
    Queued,
    AlreadyQueued,
    QueueFull,
    Disabled,
    InvalidCpu,
    WorkerUnavailable,
}

/// Result of a delayed-work schedule attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDelayedWorkResult {
    Queued,
    AlreadyQueued,
    QueueFull,
    Disabled,
    InvalidCpu,
    TimerUnavailable,
    WorkerUnavailable,
}

/// Result of a non-blocking cancel attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelWorkResult {
    /// A pending queued or delayed instance was removed.
    CancelledPending,
    /// No queued, delayed, or running instance existed.
    NotPending,
    /// The callback is already running and was not waited for.
    Running,
}

/// Wait/cancel error surfaced by synchronous APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkqueueError {
    InvalidContext,
    WouldDeadlock,
    WaitRegistrationFailed,
    Destroyed,
}

/// Error returned while disabling a work item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisableWorkError {
    Overflow,
}

/// Error returned while enabling a work item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnableWorkError {
    NotDisabled,
}

/// Error returned while allocating a dynamic queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkQueueAllocError {
    InvalidContext,
    UnsupportedFlags,
}

/// Active limit requested by a workqueue user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkQueueMaxActive(usize);

impl WorkQueueMaxActive {
    pub const fn new(value: usize) -> Self {
        Self(value)
    }

    pub const fn get(self) -> usize {
        self.0
    }
}

bitflags! {
    /// Workqueue policy flags accepted by the product API.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct WorkQueueFlags: u32 {
        const UNBOUND = 1 << 0;
        const HIGHPRI = 1 << 1;
        const BH = 1 << 2;
    }
}

/// Attributes used when allocating a dynamic queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkQueueAttrs {
    flags: WorkQueueFlags,
    max_active: WorkQueueMaxActive,
}

impl WorkQueueAttrs {
    pub const fn new() -> Self {
        Self {
            flags: WorkQueueFlags::empty(),
            max_active: WorkQueueMaxActive(DEFAULT_WORKQUEUE_MAX_ACTIVE),
        }
    }

    pub const fn flags(self) -> WorkQueueFlags {
        self.flags
    }

    pub const fn max_active(self) -> WorkQueueMaxActive {
        self.max_active
    }

    pub const fn with_flags(mut self, flags: WorkQueueFlags) -> Self {
        self.flags = flags;
        self
    }

    pub const fn with_max_active(mut self, max_active: usize) -> Self {
        self.max_active = WorkQueueMaxActive(max_active);
        self
    }
}

impl Default for WorkQueueAttrs {
    fn default() -> Self {
        Self::new()
    }
}

/// A logical workqueue product object.
pub struct WorkQueue {
    core: CoreQueue,
    backend: WorkQueueBackend,
    default_target: DefaultTargetPolicy,
    destroyed: AtomicBool,
    registered: AtomicBool,
    state_change: klazy::Once<kpoll::PollEvent>,
}

#[derive(Clone)]
pub(crate) enum WorkQueueRef {
    Static(&'static WorkQueue),
    Dynamic(Arc<WorkQueue>),
}

impl WorkQueueRef {
    pub(crate) fn queue(&self) -> &WorkQueue {
        match self {
            Self::Static(queue) => queue,
            Self::Dynamic(queue) => queue,
        }
    }

    pub(crate) fn bottom_half_kind(&self) -> Option<BottomHalfWorkQueueKind> {
        match self.queue().backend {
            WorkQueueBackend::System => None,
            WorkQueueBackend::BottomHalf(kind) => Some(kind),
        }
    }
}

#[derive(Clone)]
enum RegisteredWorkQueue {
    Static(&'static WorkQueue),
    Dynamic(Weak<WorkQueue>),
}

impl RegisteredWorkQueue {
    fn from_ref(queue: &WorkQueueRef) -> Self {
        match queue {
            WorkQueueRef::Static(queue) => Self::Static(queue),
            WorkQueueRef::Dynamic(queue) => Self::Dynamic(Arc::downgrade(queue)),
        }
    }

    fn upgrade(&self) -> Option<WorkQueueRef> {
        match self {
            Self::Static(queue) => Some(WorkQueueRef::Static(queue)),
            Self::Dynamic(queue) => queue.upgrade().map(WorkQueueRef::Dynamic),
        }
    }

    fn ptr_eq(&self, queue: &WorkQueue) -> bool {
        match self {
            Self::Static(registered) => core::ptr::eq(*registered, queue),
            Self::Dynamic(registered) => registered.as_ptr() == core::ptr::from_ref(queue),
        }
    }
}

#[derive(Clone)]
struct RegisteredBinding {
    queue: RegisteredWorkQueue,
    cpu_id: LogicalCpuId,
}

impl RegisteredBinding {
    fn upgrade(&self) -> Option<RegisteredBindingRef> {
        Some(RegisteredBindingRef {
            queue: self.queue.upgrade()?,
            cpu_id: self.cpu_id,
        })
    }
}

struct RegisteredBindingRef {
    queue: WorkQueueRef,
    cpu_id: LogicalCpuId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkQueueBackend {
    System,
    BottomHalf(BottomHalfWorkQueueKind),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DefaultTargetPolicy {
    CurrentCpu,
    BalancedReadyNormalPools,
}

impl WorkQueue {
    pub const fn new(name: &'static str) -> Self {
        Self {
            core: CoreQueue::new(name, DEFAULT_WORKQUEUE_MAX_ACTIVE),
            backend: WorkQueueBackend::System,
            default_target: DefaultTargetPolicy::CurrentCpu,
            destroyed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            state_change: klazy::Once::new(),
        }
    }

    pub(crate) const fn system(name: &'static str, _kind: SystemWorkQueueKind) -> Self {
        Self {
            core: CoreQueue::new(name, DEFAULT_WORKQUEUE_MAX_ACTIVE),
            backend: WorkQueueBackend::System,
            default_target: DefaultTargetPolicy::BalancedReadyNormalPools,
            destroyed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            state_change: klazy::Once::new(),
        }
    }

    pub(crate) const fn system_percpu(name: &'static str, _kind: SystemWorkQueueKind) -> Self {
        Self {
            core: CoreQueue::new(name, DEFAULT_WORKQUEUE_MAX_ACTIVE),
            backend: WorkQueueBackend::System,
            default_target: DefaultTargetPolicy::CurrentCpu,
            destroyed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            state_change: klazy::Once::new(),
        }
    }

    pub(crate) const fn bottom_half(name: &'static str, kind: BottomHalfWorkQueueKind) -> Self {
        Self {
            core: CoreQueue::new(name, DEFAULT_WORKQUEUE_MAX_ACTIVE),
            backend: WorkQueueBackend::BottomHalf(kind),
            default_target: DefaultTargetPolicy::CurrentCpu,
            destroyed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            state_change: klazy::Once::new(),
        }
    }

    pub const fn name(&self) -> &'static str {
        self.core.name()
    }

    pub(crate) fn core_binding(&self, cpu_id: LogicalCpuId) -> Option<CoreQueueBinding<'_>> {
        self.core.binding(cpu_id)
    }

    pub fn queue_work_on(
        &'static self,
        cpu_id: LogicalCpuId,
        work: &ScheduledWork,
    ) -> QueueWorkResult {
        self.queue_work_on_ref(WorkQueueRef::Static(self), cpu_id, work)
    }

    fn queue_work_on_ref(
        &self,
        queue_ref: WorkQueueRef,
        cpu_id: LogicalCpuId,
        work: &ScheduledWork,
    ) -> QueueWorkResult {
        if self.destroyed.load(Ordering::Acquire) {
            return QueueWorkResult::Disabled;
        }
        let Some(binding) = self.core.binding(cpu_id) else {
            return QueueWorkResult::InvalidCpu;
        };
        let pool = match self.backend {
            WorkQueueBackend::System => builtinpool::system_pool_for_cpu(cpu_id),
            WorkQueueBackend::BottomHalf(_) => {
                builtinpool::system_pool_for_kind_cpu(SystemPoolKind::Bh, cpu_id)
            }
        };
        let Some(pool) = pool else {
            return QueueWorkResult::WorkerUnavailable;
        };
        let op = match binding.queue_work(&work.inner.core) {
            Ok(kworkqueue::QueueWorkOutcome::Runnable(op))
            | Ok(kworkqueue::QueueWorkOutcome::Inactive(op)) => op,
            Ok(kworkqueue::QueueWorkOutcome::QueuedWhileRunning) => {
                register_workqueue_ref(&queue_ref);
                work.install_binding(queue_ref, cpu_id, binding.owner());
                hold_scheduled_work(work);
                return QueueWorkResult::Queued;
            }
            Err(kworkqueue::QueueWorkError::AlreadyQueued) => {
                return QueueWorkResult::AlreadyQueued;
            }
            Err(kworkqueue::QueueWorkError::Disabled) => return QueueWorkResult::Disabled,
            Err(kworkqueue::QueueWorkError::PendingFull) => return QueueWorkResult::QueueFull,
            Err(kworkqueue::QueueWorkError::NotPendingDelayed) => {
                return QueueWorkResult::AlreadyQueued;
            }
        };
        register_workqueue_ref(&queue_ref);
        work.install_binding(queue_ref, cpu_id, binding.owner());
        hold_scheduled_work(work);
        match pool.apply_executor_op(op) {
            Ok(()) => {
                if let WorkQueueBackend::BottomHalf(kind) = self.backend {
                    crate::runtime::raise_bottom_half_workqueue_on(cpu_id, kind);
                }
                QueueWorkResult::Queued
            }
            Err(error) => {
                self.rollback_queued_work(cpu_id, binding, work);
                match error {
                    crate::builtinpool::BuiltinPoolEnqueueError::QueueFull => {
                        QueueWorkResult::QueueFull
                    }
                    _ => QueueWorkResult::WorkerUnavailable,
                }
            }
        }
    }

    pub fn queue_work(&'static self, work: &ScheduledWork) -> QueueWorkResult {
        self.queue_work_on(self.default_target_cpu(), work)
    }

    fn default_target_cpu(&self) -> LogicalCpuId {
        match self.default_target {
            DefaultTargetPolicy::CurrentCpu => runtime::current_cpu_id(),
            DefaultTargetPolicy::BalancedReadyNormalPools => select_balanced_normal_pool_cpu(),
        }
    }

    fn mark_delayed_on(
        &'static self,
        cpu_id: LogicalCpuId,
        work: &ScheduledWork,
    ) -> QueueDelayedWorkResult {
        self.mark_delayed_on_ref(WorkQueueRef::Static(self), cpu_id, work)
    }

    fn mark_delayed_on_ref(
        &self,
        queue_ref: WorkQueueRef,
        cpu_id: LogicalCpuId,
        work: &ScheduledWork,
    ) -> QueueDelayedWorkResult {
        if self.destroyed.load(Ordering::Acquire) {
            return QueueDelayedWorkResult::Disabled;
        }
        let Some(binding) = self.core.binding(cpu_id) else {
            return QueueDelayedWorkResult::InvalidCpu;
        };
        if self.pool_for_cpu(cpu_id).is_none() {
            return QueueDelayedWorkResult::WorkerUnavailable;
        }
        match binding.mark_delayed(work.core()) {
            Ok(()) => {
                register_workqueue_ref(&queue_ref);
                work.install_binding(queue_ref, cpu_id, binding.owner());
                hold_scheduled_work(work);
                QueueDelayedWorkResult::Queued
            }
            Err(kworkqueue::QueueWorkError::AlreadyQueued)
            | Err(kworkqueue::QueueWorkError::NotPendingDelayed) => {
                QueueDelayedWorkResult::AlreadyQueued
            }
            Err(kworkqueue::QueueWorkError::Disabled) => QueueDelayedWorkResult::Disabled,
            Err(kworkqueue::QueueWorkError::PendingFull) => QueueDelayedWorkResult::QueueFull,
        }
    }

    fn activate_delayed_on(
        &'static self,
        cpu_id: LogicalCpuId,
        work: &ScheduledWork,
    ) -> QueueWorkResult {
        let Some(binding) = self.core.binding(cpu_id) else {
            return QueueWorkResult::InvalidCpu;
        };
        if self.destroyed.load(Ordering::Acquire) {
            self.discard_timer_pending_work(cpu_id, binding, work);
            return QueueWorkResult::Disabled;
        }
        if self.pool_for_cpu(cpu_id).is_none() {
            self.discard_timer_pending_work(cpu_id, binding, work);
            return QueueWorkResult::WorkerUnavailable;
        }
        let op = match binding.activate_delayed(work.core()) {
            Ok(kworkqueue::QueueWorkOutcome::Runnable(op))
            | Ok(kworkqueue::QueueWorkOutcome::Inactive(op)) => op,
            Ok(kworkqueue::QueueWorkOutcome::QueuedWhileRunning) => {
                return QueueWorkResult::Queued;
            }
            Err(kworkqueue::QueueWorkError::AlreadyQueued)
            | Err(kworkqueue::QueueWorkError::NotPendingDelayed) => {
                return QueueWorkResult::AlreadyQueued;
            }
            Err(kworkqueue::QueueWorkError::Disabled) => {
                self.discard_timer_pending_work(cpu_id, binding, work);
                return QueueWorkResult::Disabled;
            }
            Err(kworkqueue::QueueWorkError::PendingFull) => return QueueWorkResult::QueueFull,
        };
        match self.apply_executor_op(cpu_id, op) {
            Ok(()) => QueueWorkResult::Queued,
            Err(error) => {
                self.rollback_queued_work(cpu_id, binding, work);
                match error {
                    crate::builtinpool::BuiltinPoolEnqueueError::QueueFull => {
                        QueueWorkResult::QueueFull
                    }
                    _ => QueueWorkResult::WorkerUnavailable,
                }
            }
        }
    }

    pub fn flush(&self) -> Result<bool, WorkqueueError> {
        if runtime::is_invalid_wait_context() {
            return Err(WorkqueueError::InvalidContext);
        }
        if current_normal_worker_context()
            .is_some_and(|context| self.would_deadlock_current_worker(context))
        {
            return Err(WorkqueueError::WouldDeadlock);
        }
        let mut snapshots = Vec::new();
        for cpu in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu);
            if let Some(binding) = self.core.binding(cpu_id) {
                let snapshot = binding.start_flush();
                if !snapshot.complete() {
                    snapshots.push((binding, snapshot));
                }
            }
        }
        if snapshots.is_empty() {
            return Ok(false);
        }
        let mut ready = || {
            snapshots
                .iter()
                .all(|(binding, snapshot)| binding.flush_complete(*snapshot))
        };
        self.wait_until(&mut ready)?;
        Ok(true)
    }

    pub fn destroy(&self) -> Result<(), WorkqueueError> {
        if runtime::is_invalid_wait_context() {
            return Err(WorkqueueError::InvalidContext);
        }
        if current_normal_worker_context()
            .is_some_and(|context| self.would_deadlock_current_worker(context))
        {
            return Err(WorkqueueError::WouldDeadlock);
        }
        self.destroyed.store(true, Ordering::Release);
        self.notify_state_change();
        let _ = self.flush()?;
        unregister_workqueue(self);
        Ok(())
    }

    fn apply_executor_op(
        &self,
        cpu_id: LogicalCpuId,
        op: kworkqueue::ExecutorOp,
    ) -> Result<(), crate::builtinpool::BuiltinPoolEnqueueError> {
        if let kworkqueue::ExecutorOp::PromoteInactive { owner, budget } = op {
            let Some(binding) = self.core_binding(cpu_id) else {
                return Err(crate::builtinpool::BuiltinPoolEnqueueError::InvalidTransition);
            };
            let (pool_kind, bottom_half_kind) = match self.backend {
                WorkQueueBackend::System => (SystemPoolKind::Normal, None),
                WorkQueueBackend::BottomHalf(kind) => (SystemPoolKind::Bh, Some(kind)),
            };
            return apply_promote_inactive_for_queue(
                pool_kind,
                bottom_half_kind,
                cpu_id,
                binding,
                owner,
                budget,
            );
        }

        let pool = self.pool_for_cpu(cpu_id);
        pool.ok_or(crate::builtinpool::BuiltinPoolEnqueueError::InvalidTransition)?
            .apply_executor_op(op)?;
        if let WorkQueueBackend::BottomHalf(kind) = self.backend {
            crate::runtime::raise_bottom_half_workqueue_on(cpu_id, kind);
        }
        Ok(())
    }

    fn pool_for_cpu(&self, cpu_id: LogicalCpuId) -> Option<SystemPoolBinding> {
        match self.backend {
            WorkQueueBackend::System => builtinpool::system_pool_for_cpu(cpu_id),
            WorkQueueBackend::BottomHalf(_) => {
                builtinpool::system_pool_for_kind_cpu(SystemPoolKind::Bh, cpu_id)
            }
        }
    }

    fn would_deadlock_current_worker(
        &self,
        context: kworkerpool::WorkerPoolExecutionContext,
    ) -> bool {
        // Conservative bounded-pool guard: a system worker must not synchronously
        // wait on a queue that can target the same per-CPU worker pool. The check
        // intentionally does not inspect pending state, so it may reject a flush
        // that would be immediately complete.
        self.backend == WorkQueueBackend::System
            && self.core.binding(context.pool_id.cpu()).is_some()
    }

    fn rollback_queued_work(
        &self,
        cpu_id: LogicalCpuId,
        binding: CoreQueueBinding<'_>,
        work: &ScheduledWork,
    ) {
        if let kworkqueue::CancelPendingResult::Canceled {
            remove_op,
            promote_op,
        } = binding.cancel_pending(work.core())
        {
            work.clear_binding_if_owner(binding.owner());
            let _ = self.apply_executor_op(cpu_id, remove_op);
            if let Some(op) = promote_op {
                let _ = self.apply_executor_op(cpu_id, op);
            }
        }
        release_scheduled_work(work.core().key());
        work.notify_state_change();
        self.notify_state_change();
    }

    fn discard_timer_pending_work(
        &self,
        cpu_id: LogicalCpuId,
        binding: CoreQueueBinding<'_>,
        work: &ScheduledWork,
    ) {
        match binding.cancel_work(work.core()) {
            kworkqueue::CancelWorkResult::CanceledPending {
                remove_op,
                promote_op,
            } => {
                work.clear_binding_if_owner(binding.owner());
                let _ = self.apply_executor_op(cpu_id, remove_op);
                if let Some(op) = promote_op {
                    let _ = self.apply_executor_op(cpu_id, op);
                }
                release_scheduled_work(work.core().key());
                work.notify_state_change();
                self.notify_state_change();
            }
            kworkqueue::CancelWorkResult::CanceledRunningRequeue { remove_op } => {
                let _ = self.apply_executor_op(cpu_id, remove_op);
            }
            kworkqueue::CancelWorkResult::CanceledDelayed => {
                work.clear_binding_if_owner(binding.owner());
                release_scheduled_work(work.core().key());
                work.notify_state_change();
                self.notify_state_change();
            }
            kworkqueue::CancelWorkResult::WaitingRunning(_)
            | kworkqueue::CancelWorkResult::NotPending
            | kworkqueue::CancelWorkResult::Busy => {}
        }
    }

    fn rescue_pending_work_for_flush(
        &self,
        cpu_id: LogicalCpuId,
        binding: CoreQueueBinding<'_>,
        work: &kworkqueue::Work,
    ) {
        let Some(op) = binding.executor_op_for_pending_work(work) else {
            return;
        };
        let Some(pool) = self.pool_for_cpu(cpu_id) else {
            return;
        };
        if pool.has_executor_op_entry(op) {
            return;
        }
        let snapshot = pool.pool().lock().snapshot();
        if snapshot.nr_preparing != 0 || snapshot.nr_claiming != 0 || snapshot.nr_running_state != 0
        {
            return;
        }
        let _ = self.apply_executor_op(cpu_id, op);
    }

    pub(crate) fn notify_state_change(&self) {
        self.state_change_event().notify();
    }

    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> Result<(), WorkqueueError> {
        ktask::wait_for_poll_event_until(self.state_change_event(), &mut *ready)
            .map_err(|_| WorkqueueError::WaitRegistrationFailed)
    }

    fn state_change_event(&self) -> &kpoll::PollEvent {
        self.state_change.call_once(kpoll::PollEvent::new)
    }
}

/// Handle returned by dynamic queue allocation.
#[derive(Clone)]
pub struct WorkQueueHandle {
    queue: Arc<WorkQueue>,
}

impl WorkQueueHandle {
    pub fn alloc(name: &'static str, attrs: WorkQueueAttrs) -> Result<Self, WorkQueueAllocError> {
        if !attrs.flags().is_empty() {
            return Err(WorkQueueAllocError::UnsupportedFlags);
        }
        let queue = Arc::new(WorkQueue {
            core: CoreQueue::new(name, attrs.max_active().get()),
            backend: WorkQueueBackend::System,
            default_target: DefaultTargetPolicy::CurrentCpu,
            destroyed: AtomicBool::new(false),
            registered: AtomicBool::new(false),
            state_change: klazy::Once::new(),
        });
        register_dynamic_workqueue(&queue);
        Ok(Self { queue })
    }

    pub fn queue_work_on(&self, cpu_id: LogicalCpuId, work: &ScheduledWork) -> QueueWorkResult {
        self.queue
            .queue_work_on_ref(WorkQueueRef::Dynamic(self.queue.clone()), cpu_id, work)
    }

    pub fn queue_work(&self, work: &ScheduledWork) -> QueueWorkResult {
        self.queue.queue_work_on_ref(
            WorkQueueRef::Dynamic(self.queue.clone()),
            self.queue.default_target_cpu(),
            work,
        )
    }

    pub fn flush(&self) -> Result<bool, WorkqueueError> {
        self.queue.flush()
    }

    pub fn destroy(&self) -> Result<(), WorkqueueError> {
        self.queue.destroy()
    }
}

/// Target queue and CPU for an enqueue operation.
#[derive(Clone, Copy)]
pub struct ScheduleAttrs {
    queue: ScheduleQueueRef,
    cpu_id: Option<LogicalCpuId>,
}

impl ScheduleAttrs {
    pub const fn system() -> Self {
        Self {
            queue: ScheduleQueueRef::System(SystemWorkQueueKind::Default),
            cpu_id: None,
        }
    }

    pub const fn bottom_half() -> Self {
        Self {
            queue: ScheduleQueueRef::BottomHalf(BottomHalfWorkQueueKind::Default),
            cpu_id: None,
        }
    }

    pub const fn bottom_half_highpri() -> Self {
        Self {
            queue: ScheduleQueueRef::BottomHalf(BottomHalfWorkQueueKind::HighPri),
            cpu_id: None,
        }
    }

    pub const fn on_cpu(mut self, cpu_id: LogicalCpuId) -> Self {
        self.cpu_id = Some(cpu_id);
        self
    }

    pub(crate) const fn explicit_cpu(self) -> Option<LogicalCpuId> {
        self.cpu_id
    }

    pub(crate) const fn queue(self) -> ScheduleQueueRef {
        self.queue
    }
}

impl Default for ScheduleAttrs {
    fn default() -> Self {
        Self::system()
    }
}

/// Queue reference accepted by scheduling helpers.
#[derive(Clone, Copy)]
pub(crate) enum ScheduleQueueRef {
    System(SystemWorkQueueKind),
    BottomHalf(BottomHalfWorkQueueKind),
}

impl ScheduleQueueRef {
    pub(crate) fn queue(self) -> &'static WorkQueue {
        match self {
            Self::System(kind) => builtinwq::system_queue(kind),
            Self::BottomHalf(kind) => builtinwq::bottom_half_queue(kind),
        }
    }
}

/// Reusable callback work item.
#[derive(Clone)]
pub struct ScheduledWork {
    inner: Arc<ScheduledWorkInner>,
}

struct ScheduledWorkInner {
    core: kworkqueue::Work,
    callback: Box<dyn Fn(&ScheduledWork) + Send + Sync>,
    binding: SpinNoIrq<Option<WorkBindingLocator>>,
    state_change: kpoll::PollEvent,
}

/// Runtime-side location of the currently queued or running work instance.
///
/// `kworkqueue` owns the semantic work state; this locator only lets the public
/// `kwork` API reach the selected per-CPU binding directly for flush/cancel.
#[derive(Clone)]
struct WorkBindingLocator {
    queue: WorkQueueRef,
    cpu_id: LogicalCpuId,
    owner: kworkqueue::EntryOwner,
}

impl ScheduledWork {
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&ScheduledWork) + Send + Sync + 'static,
    {
        let work = Self {
            inner: Arc::new(ScheduledWorkInner {
                core: kworkqueue::Work::new(),
                callback: Box::new(callback),
                binding: SpinNoIrq::new(None),
                state_change: kpoll::PollEvent::new(),
            }),
        };
        register_scheduled_work(&work.inner);
        work
    }

    pub fn run(&self) {
        (self.inner.callback)(self);
    }

    pub(crate) fn core(&self) -> &kworkqueue::Work {
        &self.inner.core
    }

    pub fn schedule(&self) -> QueueWorkResult {
        self.schedule_with(ScheduleAttrs::system())
    }

    pub fn schedule_with(&self, attrs: ScheduleAttrs) -> QueueWorkResult {
        let queue = attrs.queue().queue();
        match attrs.explicit_cpu() {
            Some(cpu_id) => queue.queue_work_on(cpu_id, self),
            None => queue.queue_work(self),
        }
    }

    pub fn schedule_on_queue(&self, queue: &WorkQueueHandle) -> QueueWorkResult {
        queue.queue_work(self)
    }

    pub fn disable(&self) -> Result<usize, DisableWorkError> {
        self.core()
            .disable()
            .map_err(|kworkqueue::DisableWorkError::Overflow| DisableWorkError::Overflow)
    }

    pub fn enable(&self) -> Result<usize, EnableWorkError> {
        self.core()
            .enable()
            .map_err(|kworkqueue::EnableWorkError::NotDisabled| EnableWorkError::NotDisabled)
    }

    pub fn is_disabled(&self) -> bool {
        self.core().is_disabled()
    }

    pub fn flush(&self) -> Result<bool, WorkqueueError> {
        if runtime::is_invalid_wait_context() {
            return Err(WorkqueueError::InvalidContext);
        }
        let current_worker = current_normal_worker_context();
        let Some(locator) = self.current_binding() else {
            return Ok(false);
        };
        let queue = locator.queue.queue();
        let cpu_id = locator.cpu_id;
        let owner = locator.owner;
        let Some(binding) = queue.core_binding(cpu_id) else {
            self.clear_binding_if_owner(owner);
            return Ok(false);
        };
        let snapshot = binding.flush_work(self.core());
        if snapshot.complete() {
            self.clear_binding_if_owner(owner);
            return Ok(false);
        }
        if queue.backend == WorkQueueBackend::System
            && current_worker.is_some_and(|context| context.pool_id.cpu() == cpu_id)
        {
            return Err(WorkqueueError::WouldDeadlock);
        }
        queue.rescue_pending_work_for_flush(cpu_id, binding, self.core());
        let mut ready = || {
            if binding.flush_work_complete(snapshot, self.core()) {
                return true;
            }
            queue.rescue_pending_work_for_flush(cpu_id, binding, self.core());
            binding.flush_work_complete(snapshot, self.core())
        };
        self.wait_until(&mut ready)?;
        Ok(true)
    }

    /// Attempts to cancel this work without waiting for a running callback.
    ///
    /// Pending queued or delayed instances are removed. If the callback is
    /// already running, the work is left untouched and [`CancelWorkResult::Running`]
    /// is returned.
    pub fn cancel(&self) -> CancelWorkResult {
        let Some(locator) = self.current_binding() else {
            return CancelWorkResult::NotPending;
        };
        let queue = locator.queue.queue();
        let cpu_id = locator.cpu_id;
        let owner = locator.owner;
        let Some(binding) = queue.core_binding(cpu_id) else {
            self.clear_binding_if_owner(owner);
            return CancelWorkResult::NotPending;
        };
        match binding.cancel_work_nonblocking(self.core()) {
            kworkqueue::CancelWorkResult::CanceledPending {
                remove_op,
                promote_op,
            } => {
                self.clear_binding_if_owner(owner);
                let _ = queue.apply_executor_op(cpu_id, remove_op);
                if let Some(op) = promote_op {
                    let _ = queue.apply_executor_op(cpu_id, op);
                }
                release_scheduled_work(self.core().key());
                self.notify_state_change();
                queue.notify_state_change();
                CancelWorkResult::CancelledPending
            }
            kworkqueue::CancelWorkResult::CanceledRunningRequeue { remove_op } => {
                let _ = queue.apply_executor_op(cpu_id, remove_op);
                self.notify_state_change();
                queue.notify_state_change();
                CancelWorkResult::Running
            }
            kworkqueue::CancelWorkResult::CanceledDelayed => {
                self.clear_binding_if_owner(owner);
                release_scheduled_work(self.core().key());
                self.notify_state_change();
                queue.notify_state_change();
                CancelWorkResult::CancelledPending
            }
            kworkqueue::CancelWorkResult::WaitingRunning(_) => CancelWorkResult::Running,
            kworkqueue::CancelWorkResult::NotPending => {
                self.clear_binding_if_owner(owner);
                CancelWorkResult::NotPending
            }
            kworkqueue::CancelWorkResult::Busy => {
                self.clear_binding_if_owner(owner);
                CancelWorkResult::NotPending
            }
        }
    }

    pub fn cancel_sync(&self) -> Result<bool, WorkqueueError> {
        if runtime::is_invalid_wait_context() {
            return Err(WorkqueueError::InvalidContext);
        }
        let current_worker = current_normal_worker_context();
        let Some(locator) = self.current_binding() else {
            return Ok(false);
        };
        let queue = locator.queue.queue();
        let cpu_id = locator.cpu_id;
        let owner = locator.owner;
        let Some(binding) = queue.core_binding(cpu_id) else {
            self.clear_binding_if_owner(owner);
            return Ok(false);
        };
        match binding.cancel_work(self.core()) {
            kworkqueue::CancelWorkResult::CanceledPending {
                remove_op,
                promote_op,
            } => {
                self.clear_binding_if_owner(owner);
                let _ = queue.apply_executor_op(cpu_id, remove_op);
                if let Some(op) = promote_op {
                    let _ = queue.apply_executor_op(cpu_id, op);
                }
                release_scheduled_work(self.core().key());
                self.notify_state_change();
                queue.notify_state_change();
                Ok(true)
            }
            kworkqueue::CancelWorkResult::CanceledRunningRequeue { remove_op } => {
                let _ = queue.apply_executor_op(cpu_id, remove_op);
                self.notify_state_change();
                queue.notify_state_change();
                Ok(true)
            }
            kworkqueue::CancelWorkResult::CanceledDelayed => {
                self.clear_binding_if_owner(owner);
                release_scheduled_work(self.core().key());
                self.notify_state_change();
                queue.notify_state_change();
                Ok(true)
            }
            kworkqueue::CancelWorkResult::WaitingRunning(snapshot) => {
                if queue.backend == WorkQueueBackend::System
                    && current_worker.is_some_and(|context| context.pool_id.cpu() == cpu_id)
                {
                    return Err(WorkqueueError::WouldDeadlock);
                }
                let mut ready = || binding.flush_work_complete(snapshot, self.core());
                self.wait_until(&mut ready)?;
                self.clear_binding_if_owner(owner);
                release_scheduled_work(self.core().key());
                self.notify_state_change();
                queue.notify_state_change();
                Ok(true)
            }
            kworkqueue::CancelWorkResult::NotPending => {
                self.clear_binding_if_owner(owner);
                Ok(false)
            }
            kworkqueue::CancelWorkResult::Busy => {
                self.clear_binding_if_owner(owner);
                Ok(false)
            }
        }
    }

    fn install_binding(
        &self,
        queue: WorkQueueRef,
        cpu_id: LogicalCpuId,
        owner: kworkqueue::EntryOwner,
    ) {
        *self.inner.binding.lock() = Some(WorkBindingLocator {
            queue,
            cpu_id,
            owner,
        });
    }

    pub(crate) fn clear_binding_if_owner(&self, owner: kworkqueue::EntryOwner) {
        let mut binding = self.inner.binding.lock();
        if binding
            .as_ref()
            .is_some_and(|current| current.owner == owner)
        {
            *binding = None;
        }
    }

    fn current_binding(&self) -> Option<WorkBindingLocator> {
        self.inner.binding.lock().clone()
    }

    pub(crate) fn notify_state_change(&self) {
        self.inner.state_change.notify();
    }

    fn wait_until(&self, ready: &mut dyn FnMut() -> bool) -> Result<(), WorkqueueError> {
        ktask::wait_for_poll_event_until(&self.inner.state_change, &mut *ready)
            .map_err(|_| WorkqueueError::WaitRegistrationFailed)
    }
}

fn register_scheduled_work(inner: &Arc<ScheduledWorkInner>) {
    let work_key = inner.core.key();
    let mut registry = SCHEDULED_WORK_REGISTRY.lock();
    registry.retain(|weak| weak.upgrade().is_some());
    if registry
        .iter()
        .filter_map(Weak::upgrade)
        .any(|registered| registered.core.key() == work_key)
    {
        return;
    }
    registry.push(Arc::downgrade(inner));
}

fn hold_scheduled_work(work: &ScheduledWork) {
    let key = work.core().key();
    let mut active = ACTIVE_SCHEDULED_WORKS.lock();
    if active.iter().any(|inner| inner.core.key() == key) {
        return;
    }
    active.push(work.inner.clone());
}

pub(crate) fn release_scheduled_work(key: kworkqueue::WorkKey) {
    ACTIVE_SCHEDULED_WORKS
        .lock()
        .retain(|inner| inner.core.key() != key);
}

fn mark_registered(queue: &WorkQueue) -> bool {
    !queue.registered.swap(true, Ordering::AcqRel)
}

fn register_workqueue_ref(queue: &WorkQueueRef) {
    match queue {
        WorkQueueRef::Static(queue) => register_static_workqueue(queue),
        WorkQueueRef::Dynamic(queue) => register_dynamic_workqueue(queue),
    }
}

fn register_static_workqueue(queue: &'static WorkQueue) {
    if queue.destroyed.load(Ordering::Acquire) {
        return;
    }
    if !mark_registered(queue) {
        return;
    }
    let mut registry = WORKQUEUE_REGISTRY.lock();
    if registry.iter().any(|registered| registered.ptr_eq(queue)) {
        return;
    }
    registry.push(RegisteredWorkQueue::Static(queue));
    drop(registry);
    register_queue_bindings(&WorkQueueRef::Static(queue));
}

fn register_dynamic_workqueue(queue: &Arc<WorkQueue>) {
    if queue.destroyed.load(Ordering::Acquire) {
        return;
    }
    if !mark_registered(queue) {
        return;
    }
    let mut registry = WORKQUEUE_REGISTRY.lock();
    if registry
        .iter()
        .any(|registered| registered.ptr_eq(queue.as_ref()))
    {
        return;
    }
    registry.push(RegisteredWorkQueue::Dynamic(Arc::downgrade(queue)));
    drop(registry);
    register_queue_bindings(&WorkQueueRef::Dynamic(queue.clone()));
}

pub(crate) fn unregister_workqueue(queue: &WorkQueue) {
    queue.registered.store(false, Ordering::Release);
    WORKQUEUE_REGISTRY.lock().retain(|registered| {
        registered
            .upgrade()
            .is_some_and(|registered| !core::ptr::eq(registered.queue(), queue))
    });
    BINDING_REGISTRY.lock().retain(|_, registered| {
        registered
            .upgrade()
            .is_some_and(|registered| !core::ptr::eq(registered.queue.queue(), queue))
    });
}

fn register_queue_bindings(queue_ref: &WorkQueueRef) {
    let queue = queue_ref.queue();
    let mut registry = BINDING_REGISTRY.lock();
    for cpu in 0..kbuild_config::NR_CPUS {
        let cpu_id = LogicalCpuId::new(cpu);
        let Some(binding) = queue.core_binding(cpu_id) else {
            continue;
        };
        let id = binding.binding_id();
        registry.insert(
            id,
            RegisteredBinding {
                queue: RegisteredWorkQueue::from_ref(queue_ref),
                cpu_id,
            },
        );
    }
}

#[cfg(feature = "stress_test")]
pub(crate) fn registered_workqueues() -> Vec<WorkQueueRef> {
    register_builtin_workqueues();
    let mut registry = WORKQUEUE_REGISTRY.lock();
    let mut queues = Vec::new();
    registry.retain(|registered| {
        let Some(queue) = registered.upgrade() else {
            return false;
        };
        queues.push(queue);
        true
    });
    queues
}

fn register_builtin_workqueues() {
    for queue in [
        system_wq(),
        system_percpu_wq(),
        system_bh_wq(),
        system_bh_highpri_wq(),
    ] {
        register_static_workqueue(queue);
    }
}

fn current_normal_worker_context() -> Option<kworkerpool::WorkerPoolExecutionContext> {
    let context = ktask::current_execution_context()?;
    let context = kworkerpool::decode_task_context(context)?;
    let kind = SystemPoolKind::from_usize(context.pool_id.kind().as_usize())?;
    (kind == SystemPoolKind::Normal).then_some(context)
}

pub(crate) fn scheduled_work_by_key(key: kworkqueue::WorkKey) -> Option<ScheduledWork> {
    if let Some(inner) = ACTIVE_SCHEDULED_WORKS
        .lock()
        .iter()
        .find(|inner| inner.core.key() == key)
        .cloned()
    {
        return Some(ScheduledWork { inner });
    }
    let mut registry = SCHEDULED_WORK_REGISTRY.lock();
    let mut found = None;
    registry.retain(|weak| {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        if inner.core.key() == key {
            found = Some(ScheduledWork { inner });
        }
        true
    });
    found
}

pub(crate) fn binding_for_executor_entry(
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    entry: kworkqueue::ExecutorEntry,
) -> Option<ExecutorBinding> {
    register_builtin_workqueues();
    let registered = {
        let mut registry = BINDING_REGISTRY.lock();
        let registered = registry.get(&entry.binding).cloned()?;
        let Some(binding) = registered.upgrade() else {
            registry.remove(&entry.binding);
            return None;
        };
        binding
    };
    if registered.cpu_id != cpu_id {
        return None;
    }
    let queue_ref = registered.queue;
    let queue = queue_ref.queue();
    let bottom_half_kind = match queue.backend {
        WorkQueueBackend::System if pool_kind == SystemPoolKind::Normal => None,
        WorkQueueBackend::BottomHalf(kind) if pool_kind == SystemPoolKind::Bh => Some(kind),
        _ => return None,
    };
    let binding = queue.core_binding(registered.cpu_id)?;
    (binding.binding_id() == entry.binding && binding.owner() == entry.owner).then_some(
        ExecutorBinding {
            queue: queue_ref,
            cpu_id: registered.cpu_id,
            bottom_half_kind,
        },
    )
}

pub(crate) struct ExecutorBinding {
    pub queue: WorkQueueRef,
    pub cpu_id: LogicalCpuId,
    pub bottom_half_kind: Option<BottomHalfWorkQueueKind>,
}

pub(crate) fn apply_promote_inactive_for_queue(
    pool_kind: SystemPoolKind,
    bottom_half_kind: Option<BottomHalfWorkQueueKind>,
    cpu_id: LogicalCpuId,
    binding: CoreQueueBinding<'_>,
    owner: kworkqueue::EntryOwner,
    budget: usize,
) -> Result<(), crate::builtinpool::BuiltinPoolEnqueueError> {
    apply_promote_inactive(pool_kind, cpu_id, binding, owner, budget)?;
    if let Some(kind) = bottom_half_kind {
        crate::runtime::raise_bottom_half_workqueue_on(cpu_id, kind);
    }
    Ok(())
}

pub(crate) fn apply_promote_inactive(
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    binding: CoreQueueBinding<'_>,
    owner: kworkqueue::EntryOwner,
    budget: usize,
) -> Result<(), crate::builtinpool::BuiltinPoolEnqueueError> {
    let Some(pool) = builtinpool::system_pool_for_kind_cpu(pool_kind, cpu_id) else {
        return Err(crate::builtinpool::BuiltinPoolEnqueueError::InvalidTransition);
    };

    for _ in 0..budget {
        let now = ktask::monotonic_time();
        let Some((entry, actions)) =
            pool.promote_one_deferred_raw(crate::builtinpool::pool_owner(owner), now)?
        else {
            break;
        };

        if let Some(work_key) = binding.work_key_for_entry(entry)
            && let Some(work) = scheduled_work_by_key(work_key)
            && binding.commit_promoted(entry, work.core())
        {
            crate::builtinpool::handle_actions(actions);
        } else {
            // The pool entry became stale between queue accounting and backend
            // promotion. Leave it runnable; the normal claim path will discard
            // it without charging queue active accounting.
            crate::builtinpool::handle_actions(actions);
        }
    }
    Ok(())
}

/// Timer-triggered wrapper around a [`ScheduledWork`].
#[derive(Clone)]
pub struct DelayedScheduledWork {
    inner: Arc<DelayedScheduledWorkInner>,
}

struct DelayedScheduledWorkInner {
    work: ScheduledWork,
    timer: SpinNoIrq<Option<DelayedTimerSlot>>,
    generation: AtomicUsize,
}

enum DelayedTimerSlot {
    Arming {
        generation: usize,
    },
    Armed {
        generation: usize,
        handle: Arc<dyn WorkqueueTimerHandle>,
    },
}

impl DelayedScheduledWork {
    pub fn new<F>(callback: F) -> Self
    where
        F: Fn(&ScheduledWork) + Send + Sync + 'static,
    {
        Self {
            inner: Arc::new(DelayedScheduledWorkInner {
                work: ScheduledWork::new(callback),
                timer: SpinNoIrq::new(None),
                generation: AtomicUsize::new(1),
            }),
        }
    }

    pub fn schedule_after_with(
        &self,
        delay: TimeSpan,
        attrs: ScheduleAttrs,
    ) -> QueueDelayedWorkResult {
        let queue = attrs.queue().queue();
        let cpu_id = attrs
            .explicit_cpu()
            .unwrap_or_else(|| queue.default_target_cpu());
        if delay.is_zero() {
            return queue_work_result_to_delayed(queue.queue_work_on(cpu_id, &self.inner.work));
        }

        let result = queue.mark_delayed_on(cpu_id, &self.inner.work);
        if result != QueueDelayedWorkResult::Queued {
            return result;
        }
        let generation = self.inner.next_generation();
        *self.inner.timer.lock() = Some(DelayedTimerSlot::Arming { generation });
        let inner = self.inner.clone();
        let Some(handle) = runtime::arm_timer(
            ktask::monotonic_time() + delay,
            Arc::new(move || {
                inner.fire_timer(queue, cpu_id, generation);
            }),
        ) else {
            self.inner.fire_timer(queue, cpu_id, generation);
            return QueueDelayedWorkResult::Queued;
        };
        let mut timer = self.inner.timer.lock();
        match timer.as_mut() {
            Some(DelayedTimerSlot::Arming {
                generation: pending_generation,
            }) if *pending_generation == generation => {
                *timer = Some(DelayedTimerSlot::Armed { generation, handle });
            }
            _ => {
                let _ = handle.cancel();
            }
        }
        QueueDelayedWorkResult::Queued
    }

    pub fn mod_schedule_after_with(
        &self,
        delay: TimeSpan,
        attrs: ScheduleAttrs,
    ) -> QueueDelayedWorkResult {
        // `mod` is an enqueue-style API and may be used from contexts that
        // cannot sleep. It cancels pending state without waiting for a running
        // callback; a running callback keeps the work busy and the schedule
        // attempt reports that through the normal queue result.
        let _ = self.cancel();
        self.schedule_after_with(delay, attrs)
    }

    pub fn disable(&self) -> Result<usize, DisableWorkError> {
        self.inner.work.disable()
    }

    pub fn enable(&self) -> Result<usize, EnableWorkError> {
        self.inner.work.enable()
    }

    pub fn is_disabled(&self) -> bool {
        self.inner.work.is_disabled()
    }

    /// Attempts to cancel this delayed work without waiting.
    ///
    /// A pending timer is removed from the ktask timer wheel. Already running
    /// callbacks are reported as [`CancelWorkResult::Running`].
    pub fn cancel(&self) -> CancelWorkResult {
        let canceled_timer = {
            let mut timer = self.inner.timer.lock();
            self.inner.next_generation();
            match timer.take() {
                Some(DelayedTimerSlot::Armed { handle, .. }) => handle.cancel(),
                Some(DelayedTimerSlot::Arming { .. }) | None => false,
            }
        };
        match self.inner.work.cancel() {
            CancelWorkResult::NotPending if canceled_timer => CancelWorkResult::CancelledPending,
            result => result,
        }
    }

    pub fn cancel_sync(&self) -> Result<bool, WorkqueueError> {
        {
            let mut timer = self.inner.timer.lock();
            self.inner.next_generation();
            if let Some(DelayedTimerSlot::Armed { handle, .. }) = timer.take() {
                let _ = handle.cancel();
            }
        }
        self.inner.work.cancel_sync()
    }
}

impl DelayedScheduledWorkInner {
    fn next_generation(&self) -> usize {
        loop {
            let current = self.generation.load(Ordering::Acquire);
            let next = current
                .checked_add(1)
                .expect("delayed work generation exhausted");
            if next == 0 {
                panic!("delayed work generation exhausted");
            }
            if self
                .generation
                .compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return next;
            }
        }
    }

    fn fire_timer(&self, queue: &'static WorkQueue, cpu_id: LogicalCpuId, generation: usize) {
        {
            let mut timer = self.timer.lock();
            if self.generation.load(Ordering::Acquire) != generation {
                return;
            }
            let Some(slot_generation) = timer.as_ref().map(DelayedTimerSlot::generation) else {
                return;
            };
            if slot_generation != generation {
                return;
            }
            let _ = timer.take();
        }
        let result = queue.activate_delayed_on(cpu_id, &self.work);
        if result != QueueWorkResult::Queued {
            self.work
                .clear_binding_if_owner(binding_owner(queue, cpu_id));
            release_scheduled_work(self.work.core().key());
            self.work.notify_state_change();
            queue.notify_state_change();
        }
    }
}

fn binding_owner(queue: &'static WorkQueue, cpu_id: LogicalCpuId) -> kworkqueue::EntryOwner {
    queue
        .core_binding(cpu_id)
        .expect("timer callback uses a previously validated CPU")
        .owner()
}

impl DelayedTimerSlot {
    fn generation(&self) -> usize {
        match self {
            Self::Arming { generation } | Self::Armed { generation, .. } => *generation,
        }
    }
}

fn queue_work_result_to_delayed(result: QueueWorkResult) -> QueueDelayedWorkResult {
    match result {
        QueueWorkResult::Queued => QueueDelayedWorkResult::Queued,
        QueueWorkResult::AlreadyQueued => QueueDelayedWorkResult::AlreadyQueued,
        QueueWorkResult::QueueFull => QueueDelayedWorkResult::QueueFull,
        QueueWorkResult::Disabled => QueueDelayedWorkResult::Disabled,
        QueueWorkResult::InvalidCpu => QueueDelayedWorkResult::InvalidCpu,
        QueueWorkResult::WorkerUnavailable => QueueDelayedWorkResult::WorkerUnavailable,
    }
}

static SYSTEM_WQ_BALANCE_CURSOR: AtomicUsize = AtomicUsize::new(0);

fn select_balanced_normal_pool_cpu() -> LogicalCpuId {
    let current = runtime::current_cpu_id();
    if kbuild_config::NR_CPUS <= 1 {
        return current;
    }

    let start = SYSTEM_WQ_BALANCE_CURSOR.fetch_add(1, Ordering::Relaxed) % kbuild_config::NR_CPUS;
    for offset in 0..kbuild_config::NR_CPUS {
        let cpu_id = LogicalCpuId::new((start + offset) % kbuild_config::NR_CPUS);
        if builtinpool::is_system_worker_pool_ready(SystemPoolKind::Normal, cpu_id) {
            return cpu_id;
        }
    }
    current
}

#[cfg(unittest)]
mod tests {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ktime_types::TimeSpan;
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;
    use crate::runtime::init_system_workqueue_worker_pools_for_cpu;

    static TEST_STATIC_WQ: WorkQueue = WorkQueue::new("kwork_test_static_wq");

    const RESULT_WOULD_DEADLOCK: usize = 1;
    const RESULT_UNEXPECTED: usize = 2;

    fn ensure_normal_pool_ready(cpu_id: LogicalCpuId) {
        if builtinpool::is_system_worker_pool_ready(SystemPoolKind::Normal, cpu_id) {
            return;
        }
        if init_system_workqueue_worker_pools_for_cpu(cpu_id).is_none()
            || !builtinpool::is_system_worker_pool_ready(SystemPoolKind::Normal, cpu_id)
        {
            panic!("failed to initialize normal workqueue pool for test");
        }
    }

    fn ready_normal_cpus() -> Vec<LogicalCpuId> {
        let mut cpus = Vec::new();
        for cpu in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu);
            if builtinpool::is_system_worker_pool_ready(SystemPoolKind::Normal, cpu_id) {
                cpus.push(cpu_id);
            }
        }
        cpus
    }

    fn wait_for_atomic_value(value: &AtomicUsize, expected: usize) {
        for _ in 0..100_000 {
            if value.load(Ordering::Acquire) == expected {
                return;
            }
            ktask::yield_now();
        }
        panic!("timed out waiting for expected atomic value");
    }

    fn wait_for_atomic_at_least(value: &AtomicUsize, expected: usize) {
        for _ in 0..100_000 {
            if value.load(Ordering::Acquire) >= expected {
                return;
            }
            ktask::yield_now();
        }
        panic!("timed out waiting for expected atomic value");
    }

    fn wait_until_atomic_at_least(value: &AtomicUsize, expected: usize, retries: usize) -> bool {
        for _ in 0..retries {
            if value.load(Ordering::Acquire) >= expected {
                return true;
            }
            ktask::yield_now();
        }
        false
    }

    #[def_test(serial)]
    fn delayed_zero_delay_queues_immediately_without_timer() {
        ensure_normal_pool_ready(runtime::current_cpu_id());
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });

        assert_eq!(
            work.schedule_after_with(TimeSpan::ZERO, ScheduleAttrs::system()),
            QueueDelayedWorkResult::Queued
        );
        assert_ne!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::DelayedPending
        );
        let _ = work.inner.work.cancel_sync();
    }

    #[def_test(serial)]
    fn stale_delayed_timer_generation_does_not_activate_work() {
        let work = DelayedScheduledWork::new(|_| {});
        let queue = system_wq();
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        assert_eq!(
            queue.mark_delayed_on(cpu_id, &work.inner.work),
            QueueDelayedWorkResult::Queued
        );
        let stale_generation = work.inner.next_generation();
        work.inner.next_generation();
        work.inner.fire_timer(queue, cpu_id, stale_generation);

        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::DelayedPending
        );
        assert_eq!(
            work.inner
                .work
                .cancel_sync()
                .expect("cancel should be sleepable"),
            true
        );
    }

    #[def_test(serial)]
    fn cancel_sync_prevents_late_timer_from_requeueing() {
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });
        let queue = system_wq();
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        assert_eq!(
            queue.mark_delayed_on(cpu_id, &work.inner.work),
            QueueDelayedWorkResult::Queued
        );
        let stale_generation = work.inner.generation.load(Ordering::Acquire);
        assert_eq!(
            work.cancel_sync().expect("cancel should be sleepable"),
            true
        );

        work.inner.fire_timer(queue, cpu_id, stale_generation);
        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::Idle
        );
        assert_eq!(runs.load(Ordering::Acquire), 0);
    }

    #[def_test(serial)]
    fn cancel_pending_dynamic_work_does_not_wait() {
        let pending_runs = Arc::new(AtomicUsize::new(0));
        let blocker_started = Arc::new(AtomicUsize::new(0));
        let blocker_finish = Arc::new(AtomicUsize::new(0));
        let queue = WorkQueueHandle::alloc(
            "kwork_cancel_pending_test",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic queue should allocate");
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let started = blocker_started.clone();
        let finish = blocker_finish.clone();
        let blocker = ScheduledWork::new(move |_| {
            started.store(1, Ordering::Release);
            while finish.load(Ordering::Acquire) == 0 {
                ktask::yield_now();
            }
        });
        let runs = pending_runs.clone();
        let target = ScheduledWork::new(move |_| {
            runs.fetch_add(1, Ordering::AcqRel);
        });

        assert_eq!(
            queue.queue_work_on(cpu_id, &blocker),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&blocker_started, 1);
        assert_eq!(
            queue.queue_work_on(cpu_id, &target),
            QueueWorkResult::Queued
        );
        assert_eq!(target.cancel(), CancelWorkResult::CancelledPending);
        assert_eq!(pending_runs.load(Ordering::Acquire), 0);
        blocker_finish.store(1, Ordering::Release);
        let _ = blocker.flush().expect("blocker flush should be sleepable");
        assert_eq!(queue.flush(), Ok(false));
        queue.destroy().expect("destroy after flush should succeed");
    }

    #[def_test(serial)]
    fn cancel_running_work_does_not_wait() {
        let blocker_started = Arc::new(AtomicUsize::new(0));
        let blocker_finish = Arc::new(AtomicUsize::new(0));
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let started = blocker_started.clone();
        let finish = blocker_finish.clone();
        let work = ScheduledWork::new(move |_| {
            started.store(1, Ordering::Release);
            while finish.load(Ordering::Acquire) == 0 {
                ktask::yield_now();
            }
        });

        assert_eq!(
            system_percpu_wq().queue_work_on(cpu_id, &work),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&blocker_started, 1);
        assert_eq!(work.cancel(), CancelWorkResult::Running);
        blocker_finish.store(1, Ordering::Release);
        let _ = work.flush().expect("work flush should be sleepable");
    }

    #[def_test(serial)]
    fn sleeping_worker_releases_pool_concurrency_for_queued_work() {
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        let blocker_started = Arc::new(AtomicUsize::new(0));
        let release_blocker = Arc::new(AtomicBool::new(false));
        let blocker_event = Arc::new(kpoll::PollEvent::new());
        let progress_runs = Arc::new(AtomicUsize::new(0));

        let started = blocker_started.clone();
        let release = release_blocker.clone();
        let event = blocker_event.clone();
        let blocker = ScheduledWork::new(move |_| {
            started.store(1, Ordering::Release);
            ktask::wait_for_poll_event_until(&event, || release.load(Ordering::Acquire))
                .expect("blocker wait should register");
        });

        let runs = progress_runs.clone();
        let release = release_blocker.clone();
        let event = blocker_event.clone();
        let progress = ScheduledWork::new(move |_| {
            runs.fetch_add(1, Ordering::AcqRel);
            release.store(true, Ordering::Release);
            event.notify();
        });

        assert_eq!(
            system_wq().queue_work_on(cpu_id, &blocker),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&blocker_started, 1);
        assert_eq!(
            system_wq().queue_work_on(cpu_id, &progress),
            QueueWorkResult::Queued
        );

        let progressed = wait_until_atomic_at_least(&progress_runs, 1, 100_000);
        if !progressed {
            release_blocker.store(true, Ordering::Release);
            blocker_event.notify();
        }
        let _ = blocker.flush().expect("blocker flush should be sleepable");
        let _ = progress
            .flush()
            .expect("progress flush should be sleepable");

        assert!(
            progressed,
            "sleeping worker did not release worker-pool concurrency for queued work"
        );
    }

    #[def_test(serial)]
    fn cancel_running_requeue_reports_running() {
        let started = Arc::new(AtomicUsize::new(0));
        let requeued = Arc::new(AtomicUsize::new(0));
        let finish = Arc::new(AtomicUsize::new(0));
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let started_ref = started.clone();
        let requeued_ref = requeued.clone();
        let finish_ref = finish.clone();
        let work = ScheduledWork::new(move |work| {
            started_ref.store(1, Ordering::Release);
            if system_percpu_wq().queue_work_on(cpu_id, work) == QueueWorkResult::Queued {
                requeued_ref.store(1, Ordering::Release);
            }
            while finish_ref.load(Ordering::Acquire) == 0 {
                ktask::yield_now();
            }
        });

        assert_eq!(
            system_percpu_wq().queue_work_on(cpu_id, &work),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&started, 1);
        wait_for_atomic_value(&requeued, 1);
        assert_eq!(work.cancel(), CancelWorkResult::Running);
        finish.store(1, Ordering::Release);
        let _ = work.flush().expect("work flush should be sleepable");
    }

    #[def_test(serial)]
    fn delayed_nonzero_timer_cancel_does_not_wait_or_activate_work() {
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        assert_eq!(
            work.schedule_after_with(
                TimeSpan::from_millis(20),
                ScheduleAttrs::system().on_cpu(cpu_id)
            ),
            QueueDelayedWorkResult::Queued
        );
        assert_eq!(work.cancel(), CancelWorkResult::CancelledPending);
        ktask::sleep(TimeSpan::from_millis(30));
        assert_eq!(runs.load(Ordering::Acquire), 0);
        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::Idle
        );
    }

    #[def_test(serial)]
    fn mod_delayed_work_replaces_pending_timer_without_waiting() {
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let attrs = ScheduleAttrs::system().on_cpu(cpu_id);

        assert_eq!(
            work.schedule_after_with(TimeSpan::from_millis(20), attrs),
            QueueDelayedWorkResult::Queued
        );
        assert_eq!(
            work.mod_schedule_after_with(TimeSpan::ZERO, attrs),
            QueueDelayedWorkResult::Queued
        );
        wait_for_atomic_value(&runs, 1);
        ktask::sleep(TimeSpan::from_millis(30));
        assert_eq!(runs.load(Ordering::Acquire), 1);
        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::Idle
        );
    }

    #[def_test(serial)]
    fn delayed_nonzero_timer_cancel_does_not_activate_work() {
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        assert_eq!(
            work.schedule_after_with(
                TimeSpan::from_millis(20),
                ScheduleAttrs::system().on_cpu(cpu_id)
            ),
            QueueDelayedWorkResult::Queued
        );
        assert!(work.cancel_sync().expect("cancel should be sleepable"));
        ktask::sleep(TimeSpan::from_millis(30));
        assert_eq!(runs.load(Ordering::Acquire), 0);
        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::Idle
        );
    }

    #[def_test(serial)]
    fn delayed_nonzero_timer_activates_work() {
        let runs = Arc::new(AtomicUsize::new(0));
        let work_runs = runs.clone();
        let work = DelayedScheduledWork::new(move |_| {
            work_runs.fetch_add(1, Ordering::AcqRel);
        });
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);

        assert_eq!(
            work.schedule_after_with(
                TimeSpan::from_millis(1),
                ScheduleAttrs::system().on_cpu(cpu_id)
            ),
            QueueDelayedWorkResult::Queued
        );
        wait_for_atomic_value(&runs, 1);
        let _ = work.inner.work.flush();
        assert!(work.inner.timer.lock().is_none());
        assert_eq!(
            work.inner.work.core().status(),
            kworkqueue::WorkStatus::Idle
        );
    }

    #[def_test(serial)]
    fn worker_callback_flush_self_returns_would_deadlock() {
        let self_flush_result = Arc::new(AtomicUsize::new(0));
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let result_slot = self_flush_result.clone();
        let work = ScheduledWork::new(move |work| {
            let result = match work.flush() {
                Err(WorkqueueError::WouldDeadlock) => RESULT_WOULD_DEADLOCK,
                _ => RESULT_UNEXPECTED,
            };
            result_slot.store(result, Ordering::Release);
        });

        assert_eq!(
            system_percpu_wq().queue_work_on(cpu_id, &work),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&self_flush_result, RESULT_WOULD_DEADLOCK);
        assert_eq!(
            self_flush_result.load(Ordering::Acquire),
            RESULT_WOULD_DEADLOCK
        );
        let _ = work.flush();
    }

    #[def_test(serial)]
    fn worker_callback_cancel_self_returns_would_deadlock() {
        let self_cancel_result = Arc::new(AtomicUsize::new(0));
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let result_slot = self_cancel_result.clone();
        let work = ScheduledWork::new(move |work| {
            let result = match work.cancel_sync() {
                Err(WorkqueueError::WouldDeadlock) => RESULT_WOULD_DEADLOCK,
                _ => RESULT_UNEXPECTED,
            };
            result_slot.store(result, Ordering::Release);
        });

        assert_eq!(
            system_percpu_wq().queue_work_on(cpu_id, &work),
            QueueWorkResult::Queued
        );
        wait_for_atomic_value(&self_cancel_result, RESULT_WOULD_DEADLOCK);
        assert_eq!(
            self_cancel_result.load(Ordering::Acquire),
            RESULT_WOULD_DEADLOCK
        );
        let _ = work.flush();
    }

    #[def_test(serial)]
    fn worker_callback_queue_flush_returns_would_deadlock() {
        let queue_flush_result = Arc::new(AtomicUsize::new(0));
        let queue = WorkQueueHandle::alloc(
            "kwork_queue_flush_deadlock_test",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic queue should allocate");
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let result_slot = queue_flush_result.clone();
        let callback_queue = queue.clone();
        let work = ScheduledWork::new(move |_| {
            let result = match callback_queue.flush() {
                Err(WorkqueueError::WouldDeadlock) => RESULT_WOULD_DEADLOCK,
                _ => RESULT_UNEXPECTED,
            };
            result_slot.store(result, Ordering::Release);
        });

        assert_eq!(queue.queue_work_on(cpu_id, &work), QueueWorkResult::Queued);
        wait_for_atomic_value(&queue_flush_result, RESULT_WOULD_DEADLOCK);
        let _ = work.flush();
        queue.destroy().expect("destroy after flush should succeed");
    }

    #[def_test(serial)]
    fn worker_callback_queue_destroy_returns_would_deadlock() {
        let queue_destroy_result = Arc::new(AtomicUsize::new(0));
        let after_runs = Arc::new(AtomicUsize::new(0));
        let queue = WorkQueueHandle::alloc(
            "kwork_queue_destroy_deadlock_test",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic queue should allocate");
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let result_slot = queue_destroy_result.clone();
        let callback_queue = queue.clone();
        let work = ScheduledWork::new(move |_| {
            let result = match callback_queue.destroy() {
                Err(WorkqueueError::WouldDeadlock) => RESULT_WOULD_DEADLOCK,
                _ => RESULT_UNEXPECTED,
            };
            result_slot.store(result, Ordering::Release);
        });
        let runs = after_runs.clone();
        let after = ScheduledWork::new(move |_| {
            runs.fetch_add(1, Ordering::AcqRel);
        });

        assert_eq!(queue.queue_work_on(cpu_id, &work), QueueWorkResult::Queued);
        wait_for_atomic_value(&queue_destroy_result, RESULT_WOULD_DEADLOCK);
        let _ = work.flush();
        assert_eq!(queue.queue_work_on(cpu_id, &after), QueueWorkResult::Queued);
        wait_for_atomic_at_least(&after_runs, 1);
        queue.destroy().expect("destroy after flush should succeed");
    }

    #[def_test(serial)]
    fn dynamic_queue_flush_runs_promoted_inactive_work() {
        let runs = Arc::new(AtomicUsize::new(0));
        let queue = WorkQueueHandle::alloc(
            "kwork_promote_inactive_test",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic queue should allocate");
        let first_runs = runs.clone();
        let first = ScheduledWork::new(move |_| {
            first_runs.fetch_add(1, Ordering::AcqRel);
        });
        let second_runs = runs.clone();
        let second = ScheduledWork::new(move |_| {
            second_runs.fetch_add(1, Ordering::AcqRel);
        });

        let cpu_id = LogicalCpuId::new(0);
        ensure_normal_pool_ready(cpu_id);
        assert_eq!(queue.queue_work_on(cpu_id, &first), QueueWorkResult::Queued);
        assert_eq!(
            queue.queue_work_on(cpu_id, &second),
            QueueWorkResult::Queued
        );
        assert!(queue.flush().expect("queue flush should be sleepable"));
        assert_eq!(runs.load(Ordering::Acquire), 2);
        queue.destroy().expect("destroy after flush should succeed");
    }

    #[def_test(serial)]
    fn static_queue_flush_claims_registered_owner() {
        let runs = Arc::new(AtomicUsize::new(0));
        let cpu_id = runtime::current_cpu_id();
        ensure_normal_pool_ready(cpu_id);
        let mut batch = Vec::new();
        for _ in 0..8 {
            let runs = runs.clone();
            batch.push(ScheduledWork::new(move |_| {
                runs.fetch_add(1, Ordering::AcqRel);
            }));
        }

        for work in &batch {
            assert_eq!(
                TEST_STATIC_WQ.queue_work_on(cpu_id, work),
                QueueWorkResult::Queued
            );
        }
        let _ = TEST_STATIC_WQ
            .flush()
            .expect("static queue flush should be sleepable");
        assert_eq!(runs.load(Ordering::Acquire), 8);
    }

    #[def_test(serial)]
    fn builtin_system_wq_default_selector_spreads_ready_normal_pools() {
        let ready = ready_normal_cpus();
        if ready.len() >= 2 {
            let mut seen = [false; kbuild_config::NR_CPUS];
            for _ in 0..(ready.len() * 4) {
                let cpu_id = system_wq().default_target_cpu();
                seen[cpu_id.as_usize()] = true;
            }

            assert!(seen.iter().filter(|seen| **seen).count() >= 2);
        }
    }
}
