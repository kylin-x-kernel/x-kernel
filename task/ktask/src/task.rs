// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core task data structures and lifecycle helpers.

use alloc::{boxed::Box, string::String, sync::Arc};
#[cfg(feature = "snapshot")]
use core::sync::atomic::AtomicU64;
use core::{
    alloc::Layout,
    any::Any,
    cell::{Cell, UnsafeCell},
    fmt,
    future::poll_fn,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicUsize, Ordering},
    task::Poll,
};

#[cfg(feature = "smp")]
use kcpu_id_map::KCpuMaskExt;
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use khal::context::TaskContext;
#[cfg(feature = "tls")]
use khal::tls::TlsArea;
use kpoll::{Completion, PollRegistrations, PollSet};
use kspin::SpinNoIrq;
use memaddr::{VirtAddr, align_up_4k};

use crate::{KCpuMask, KTask, KtaskRef, future::block_on, yield_now};

enum TaskIdentity {
    Idle,
    Internal,
    KernelThread {
        task_number: Arc<kidentity::PidHandle>,
    },
    User {
        task_number: Arc<kidentity::PidHandle>,
        user_runtime: UserRuntimeSlot,
    },
}

impl TaskIdentity {
    fn thread_pid(&self) -> Option<&Arc<kidentity::PidHandle>> {
        match self {
            Self::KernelThread { task_number, .. } | Self::User { task_number, .. } => {
                Some(task_number)
            }
            Self::Idle | Self::Internal => None,
        }
    }

    fn trace_id(&self) -> u64 {
        match self {
            Self::Idle | Self::Internal => 0,
            Self::KernelThread { task_number, .. } | Self::User { task_number, .. } => {
                task_number.root_nr() as u64
            }
        }
    }

    fn user_runtime(&self) -> Option<&dyn UserTaskRuntime> {
        match self {
            Self::User { user_runtime, .. } => user_runtime.get(),
            Self::Idle | Self::Internal | Self::KernelThread { .. } => None,
        }
    }
}

/// The possible states of a task.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum TaskState {
    /// Task is running on some CPU.
    Running = 1,
    /// Task is ready to run on some scheduler's ready queue.
    Ready   = 2,
    /// Task is blocked (in the wait queue or timer list),
    /// and it has finished its scheduling process, it can be wake up by `notify()` on any run queue safely.
    Blocked = 3,
    /// Task is exited and waiting for being dropped.
    Exited  = 4,
}

/// Scheduler callbacks and type-erased access for a user-task runtime.
///
/// The scheduler invokes these callbacks with preemption disabled while
/// switching tasks. Implementations must not sleep, block, recursively schedule,
/// or depend on ordinary current-process state. Shared state accessed by
/// callbacks must be synchronized for concurrent scheduler activity.
pub trait UserTaskRuntime: Send + Sync + Any {
    /// Called when the task is switched in.
    fn on_enter(&self) {}
    /// Called when the task is switched out.
    fn on_leave(&self) {}
    /// Marks that the current CPU may retain TLB state for this task's user
    /// address space.
    fn set_user_mm_resident_cpu(&self, _cpu_id: LogicalCpuId) {}
    /// Returns the latest valid hardware user page-table root for switch-in.
    fn switch_page_table_root(&self) -> Option<karch::HwPageTableRoot> {
        None
    }

    /// Returns this runtime as [`Any`] for process-domain type recovery.
    fn as_any(&self) -> &dyn Any;
}

/// Holds the user runtime attached to a `User`-identity task.
///
/// A user task always carries its runtime from construction (see
/// [`TaskInner::new_user`]); there is no longer an empty state that gets filled
/// in later, so this is effectively an immutable, single-shot container.
struct UserRuntimeSlot {
    value: UnsafeCell<Option<Box<dyn UserTaskRuntime>>>,
}

impl UserRuntimeSlot {
    fn ready(user_runtime: Box<dyn UserTaskRuntime>) -> Self {
        Self {
            value: UnsafeCell::new(Some(user_runtime)),
        }
    }

    fn get(&self) -> Option<&dyn UserTaskRuntime> {
        // SAFETY: this type does not establish synchronization on its own. Its
        // soundness relies on the surrounding invariant that a `UserRuntimeSlot`
        // is populated by `ready()` before the owning `TaskInner` is shared, and
        // that sharing happens via `Arc<TaskInner>` publication — the `Arc`
        // atomic ref-count store provides the happens-before edge that lets
        // readers on other threads observe the value written here. Once
        // published the runtime is never replaced, so the returned shared
        // reference remains valid for the task's lifetime.
        unsafe { (&*self.value.get()).as_deref() }
    }
}

// SAFETY: the runtime is set once before the task is shared and is never
// mutated afterwards; readers only observe it through shared references.
unsafe impl Send for UserRuntimeSlot {}
// SAFETY: see `Send` above — the runtime is immutable after construction and
// only shared references are exposed, so concurrent access is sound.
unsafe impl Sync for UserRuntimeSlot {}

// How many held locks we track per task (debug only).
#[cfg(feature = "snapshot")]
const HELD_LOCK_SLOTS: usize = 4;
#[cfg(feature = "snapshot")]
type HeldLocks = [AtomicUsize; HELD_LOCK_SLOTS];

#[cfg(feature = "snapshot")]
struct PerTaskRecording {
    /// 0 = not waiting, otherwise lock address.
    waiting_lock: AtomicUsize,
    /// Timer counter sample captured when waiting on `waiting_lock` began.
    waiting_since: AtomicU64,
    held_locks: HeldLocks,
}

#[cfg(feature = "snapshot")]
impl PerTaskRecording {
    fn new() -> Self {
        Self {
            waiting_lock: AtomicUsize::new(0),
            waiting_since: AtomicU64::new(0),
            held_locks: [const { AtomicUsize::new(0) }; HELD_LOCK_SLOTS],
        }
    }
}

struct TaskContextCell(UnsafeCell<TaskContext>);

impl TaskContextCell {
    fn new() -> Self {
        Self(UnsafeCell::new(TaskContext::new()))
    }

    fn get_mut(&mut self) -> &mut TaskContext {
        self.0.get_mut()
    }

    fn get(&self) -> &TaskContext {
        // SAFETY: mutable access to the stored task context is restricted to
        // `&mut self` construction paths and scheduler-controlled context
        // switch paths. Ordinary callers only observe it through a shared
        // reference.
        unsafe { &*self.0.get() }
    }

    fn as_mut_ptr(&self) -> *mut TaskContext {
        self.0.get()
    }
}

// SAFETY: `TaskContextCell` only exposes shared references for inspection and
// raw pointers for scheduler-controlled context switching. The scheduler is
// responsible for ensuring exclusive mutable access when using `as_mut_ptr`.
unsafe impl Send for TaskContextCell {}
// SAFETY: sharing `TaskContextCell` is sound because interior mutation is not
// exposed directly; callers need scheduler-controlled raw-pointer access to
// mutate the stored `TaskContext`.
unsafe impl Sync for TaskContextCell {}

/// The inner task structure.
pub struct TaskInner {
    identity: TaskIdentity,
    name: SpinNoIrq<String>,
    is_init: bool,

    entry: Cell<Option<Box<dyn FnOnce()>>>,
    state: AtomicU8,

    /// CPU affinity mask.
    cpumask: SpinNoIrq<KCpuMask>,

    /// Used to indicate the CPU ID where the task is running or will run.
    cpu_id: AtomicU32,
    /// Used to indicate whether the task is running on a CPU.
    #[cfg(feature = "smp")]
    on_cpu: AtomicBool,
    /// Wakeup metadata handed from the waker to the CPU completing switch-out.
    ///
    /// Bit 0 means the task still needs to be enqueued, bit 1 carries
    /// `WF_SYNC`, and bit 2 carries the reschedule request.
    /// SeqCst with [`Self::on_cpu`]: see [`Self::arm_wake_enqueue`].
    #[cfg(feature = "smp")]
    wake_enqueue_flags: AtomicU8,

    /// Bitmask of CPUs this task has been scheduled on since the last TLB
    /// shootdown. Used to limit the scope of cross-CPU TLB invalidation.
    #[cfg(feature = "smp")]
    on_cpu_mask: SpinNoIrq<KCpuMask>,

    #[cfg(feature = "preempt")]
    need_resched: AtomicBool,
    #[cfg(feature = "preempt")]
    preempt_disable_count: AtomicUsize,

    /// Nesting depth for Linux-style `WF_SYNC` wake scopes ([`crate::with_wake_sync`]).
    /// When non-zero, wakees may sync-preempt an eligible next-buddy on the
    /// target run queue (waker expects to sleep soon).
    wake_sync_depth: AtomicUsize,

    interrupted: AtomicBool,
    interrupt_waker: PollSet,

    /// Opaque KIRQ workerqueue callback context currently executed by this task, if any.
    ///
    /// Worker callbacks are sleepable and can migrate, so workerqueue
    /// self-wait detection must be task-local rather than per-CPU.
    workerqueue_current_work_key: AtomicUsize,
    workerqueue_current_queue_key: AtomicUsize,

    exit_code: AtomicI32,
    wait_for_exit: Completion,

    kstack: Option<TaskStack>,
    ctx: TaskContextCell,

    #[cfg(feature = "tls")]
    tls: TlsArea,

    /// Per-task watchdog recording (lock-free/NMI-safe).
    #[cfg(feature = "snapshot")]
    record_lock: PerTaskRecording,
}

impl From<u8> for TaskState {
    #[inline]
    fn from(state: u8) -> Self {
        match state {
            1 => Self::Running,
            2 => Self::Ready,
            3 => Self::Blocked,
            4 => Self::Exited,
            _ => unreachable!(),
        }
    }
}

// SAFETY: `TaskInner` is only shared through synchronization primitives for
// its interior mutable fields; raw context access is gated by scheduler/task
// invariants in this module.
unsafe impl Send for TaskInner {}
// SAFETY: shared references do not permit unsynchronized mutation beyond the
// interior-mutability primitives that already enforce the task invariants.
unsafe impl Sync for TaskInner {}

impl TaskInner {
    fn new_with_identity<F>(
        entry: F,
        name: String,
        stack_size: usize,
        identity: TaskIdentity,
    ) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        let mut t = Self::new_common(identity, name);
        debug!("new task: {}", t.id_name());
        let kstack = TaskStack::alloc(align_up_4k(stack_size));

        #[cfg(feature = "tls")]
        let tls = VirtAddr::from(t.tls.tls_ptr() as usize);
        #[cfg(not(feature = "tls"))]
        let tls = VirtAddr::from(0);

        t.entry = Cell::new(Some(Box::new(entry)));
        t.ctx_mut()
            .init(task_entry as *const () as usize, kstack.top(), tls);
        t.kstack = Some(kstack);
        t
    }

    /// Creates a scheduler-internal helper task.
    pub(crate) fn new_internal<F>(entry: F, name: String, stack_size: usize) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_with_identity(entry, name, stack_size, TaskIdentity::Internal)
    }

    /// Creates a PID-less kernel thread.
    ///
    /// Unlike [`Self::new_kthread`], this task carries an `Internal` identity and
    /// therefore does **not** allocate a root PID handle — it mirrors FreeBSD's
    /// `kthread_add`, which attaches a kernel worker to the kernel's PID-0
    /// process rather than giving it its own PID. This keeps the ordinary PID
    /// number space reserved for user processes and explicitly visible kernel
    /// threads.
    ///
    /// Use this for ordinary kernel worker threads (RX/TX pollers, deferred
    /// handlers, lazy initializers) and for the single boot-time thread that
    /// runs late subsystem initialization. The thread is otherwise a normal
    /// runnable kernel-context task: it gets time-sliced, may block, and exits
    /// normally when its work is done.
    pub fn new_pidless_kthread<F>(entry: F, name: String, stack_size: usize) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_internal(entry, name, stack_size)
    }

    /// Creates a Linux-visible kernel thread in the root/default PID namespace.
    ///
    /// This constructor is for `ktask`-owned kernel-thread creation paths.
    /// User-thread identity allocation must stay in the process domain and use
    /// [`Self::new_user`] with a preallocated [`kidentity::PidHandle`].
    pub fn new_kthread<F>(entry: F, name: String, stack_size: usize) -> KResult<Self>
    where
        F: FnOnce() + Send + 'static,
    {
        Ok(Self::new_with_identity(
            entry,
            name,
            stack_size,
            TaskIdentity::KernelThread {
                task_number: kidentity::allocate_root_pid_handle()?,
            },
        ))
    }

    /// Creates a user thread with a preallocated thread identity and runtime.
    ///
    /// Callers must allocate the thread identity in the correct PID namespace
    /// before constructing the task, so process/thread-group/publication state
    /// can be built around the same handle before the task becomes runnable.
    /// User tasks always receive the kernel-configured kernel stack size;
    /// callers cannot weaken that execution invariant.
    /// The runtime is installed during construction;
    /// a user task can therefore never be observed without its runtime.
    pub fn new_user<F>(
        entry: F,
        name: String,
        task_number: Arc<kidentity::PidHandle>,
        user_runtime: Box<dyn UserTaskRuntime>,
    ) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_with_identity(
            entry,
            name,
            kbuild_config::KERNEL_STACK_SIZE,
            TaskIdentity::User {
                task_number,
                user_runtime: UserRuntimeSlot::ready(user_runtime),
            },
        )
    }

    /// Returns the shared task-number handle, if the task has one.
    pub fn task_number(&self) -> Option<&Arc<kidentity::PidHandle>> {
        self.identity.thread_pid()
    }

    /// Returns the Linux-style root/global trace identifier.
    pub fn trace_id(&self) -> u64 {
        self.identity.trace_id()
    }

    /// Returns the scheduler-internal owner key.
    pub fn owner_key(&self) -> u64 {
        self.task_number()
            .map(|task_number| task_number.root_nr() as u64)
            .unwrap_or_else(|| self as *const _ as usize as u64)
    }

    /// Gets the name of the task.
    pub fn name(&self) -> String {
        self.name.lock().clone()
    }

    /// Set the name of the task.
    pub fn set_name(&self, name: &str) {
        *self.name.lock() = String::from(name);
    }

    /// Get a combined string of the task ID and name.
    pub fn id_name(&self) -> alloc::string::String {
        alloc::format!("Task({}, {:?})", self.owner_key(), self.name())
    }

    /// Wait for the task to exit, and return the exit code.
    ///
    /// It will return immediately if the task has already exited (but not dropped).
    pub fn join(&self) -> i32 {
        let mut registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            loop {
                if self.state() == TaskState::Exited {
                    return Poll::Ready(self.exit_code.load(Ordering::Acquire));
                }
                let mut context = registrations.context(cx);
                if self.wait_for_exit.register(&mut context).is_err() {
                    drop(context);
                    // Under memory pressure, yield and retry rather than
                    // busy-spinning on `wake_by_ref` without a registration.
                    yield_now();
                    continue;
                }
                drop(context);
                if self.state() == TaskState::Exited {
                    return Poll::Ready(self.exit_code.load(Ordering::Acquire));
                }
                return Poll::Pending;
            }
        }))
    }

    /// Returns the runtime attached to this task, if any.
    pub fn user_runtime(&self) -> Option<&dyn UserTaskRuntime> {
        self.identity.user_runtime()
    }

    /// Returns a mutable reference to the task context.
    #[inline]
    pub fn ctx_mut(&mut self) -> &mut TaskContext {
        self.ctx.get_mut()
    }

    /// Returns a shared reference to the task context.
    #[inline]
    pub fn ctx(&self) -> &TaskContext {
        self.ctx.get()
    }

    /// Returns the top address of the kernel stack.
    #[inline]
    pub fn kernel_stack_top(&self) -> Option<VirtAddr> {
        self.kstack.as_ref().map(|s| s.top())
    }

    /// Returns the owner CPU of this task's run queue.
    ///
    /// While the task is runnable or running, this is the CPU whose run queue
    /// currently owns the task. While blocked, it retains the last owner CPU
    /// and is used as the wake-affinity preference by
    /// [`crate::select_wake_run_queue`].
    #[inline]
    pub fn cpu_id(&self) -> LogicalCpuId {
        LogicalCpuId::new(self.cpu_id.load(Ordering::Acquire) as usize)
    }

    /// Gets the cpu affinity mask of the task.
    ///
    /// Returns the cpu affinity mask of the task in type [`KCpuMask`].
    #[inline]
    pub fn cpumask(&self) -> KCpuMask {
        *self.cpumask.lock()
    }

    /// Sets the cpu affinity mask of the task.
    ///
    /// # Arguments
    /// `cpumask` - The cpu affinity mask to be set in type [`KCpuMask`].
    #[inline]
    pub fn set_cpumask(&self, cpumask: KCpuMask) {
        *self.cpumask.lock() = cpumask
    }

    /// Polls whether the task has been interrupted.
    #[inline]
    pub fn poll_interrupt(
        &self,
        context: &mut kpoll::PollContext<'_>,
    ) -> Result<Poll<()>, kpoll::PollRegisterError> {
        if self.interrupted.swap(false, Ordering::AcqRel) {
            Ok(Poll::Ready(()))
        } else {
            context.register(&self.interrupt_waker)?;
            if self.interrupted.swap(false, Ordering::AcqRel) {
                Ok(Poll::Ready(()))
            } else {
                Ok(Poll::Pending)
            }
        }
    }

    /// Clears the interrupt state of the task.
    #[inline]
    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Interrupts the task.
    ///
    /// The wake uses [`crate::with_wake_sync`] when a current task exists:
    /// signal senders typically block soon after, matching Linux `WF_SYNC`.
    #[inline]
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        if crate::current_may_uninit().is_some() {
            crate::with_wake_sync(|| {
                self.interrupt_waker.wake();
            });
        } else {
            self.interrupt_waker.wake();
        }
    }

    pub(crate) fn set_workerqueue_current_work_context(
        &self,
        work_key: usize,
        queue_key: usize,
    ) -> Option<(usize, usize)> {
        let previous_queue = self
            .workerqueue_current_queue_key
            .swap(queue_key, Ordering::AcqRel);
        let previous_work = self
            .workerqueue_current_work_key
            .swap(work_key, Ordering::AcqRel);
        (previous_work != 0).then_some((previous_work, previous_queue))
    }

    pub(crate) fn clear_workerqueue_current_work_context(
        &self,
        work_key: usize,
        queue_key: usize,
    ) -> bool {
        if self.workerqueue_current_queue_key.load(Ordering::Acquire) != queue_key {
            return false;
        }
        let cleared = self
            .workerqueue_current_work_key
            .compare_exchange(work_key, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if cleared {
            self.workerqueue_current_queue_key
                .store(0, Ordering::Release);
        }
        cleared
    }

    pub(crate) fn workerqueue_current_work_context(&self) -> Option<(usize, usize)> {
        let work_key = self.workerqueue_current_work_key.load(Ordering::Acquire);
        if work_key == 0 {
            return None;
        }
        let queue_key = self.workerqueue_current_queue_key.load(Ordering::Acquire);
        Some((work_key, queue_key))
    }

    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn set_waiting_lock(&self, lock: usize, now: khal::time::TimerTicks) {
        // Publish `since` first, then `lock` with Release so readers that see
        // a non-zero lock also see the matching `since`.
        self.record_lock
            .waiting_since
            .store(now.as_raw(), Ordering::Relaxed);
        self.record_lock.waiting_lock.store(lock, Ordering::Release);
    }

    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn clear_waiting_lock(&self) {
        // Clear `lock` first (Release) so readers won't observe a stale lock
        // paired with a reset `since`.
        self.record_lock.waiting_lock.store(0, Ordering::Release);
        self.record_lock.waiting_since.store(0, Ordering::Relaxed);
    }

    /// A lock-free snapshot of the lock-wait state, safe for NMI/watchdog paths.
    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn waiting_snapshot(&self) -> Option<(usize, khal::time::TimerTicks)> {
        let lock = self.record_lock.waiting_lock.load(Ordering::Acquire);
        if lock == 0 {
            return None;
        }
        // Since lock is observed with Acquire and stored with Release, this
        // relaxed load is ordered after the lock read and should see the
        // corresponding `since` in practice.
        let since = self.record_lock.waiting_since.load(Ordering::Relaxed);
        Some((lock, khal::time::TimerTicks::from_raw(since)))
    }

    /// Getter: current waiting lock address (0 means none).
    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn waiting_lock(&self) -> usize {
        self.record_lock.waiting_lock.load(Ordering::Acquire)
    }

    /// Returns the timer counter sample captured when lock waiting began.
    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn waiting_since(&self) -> khal::time::TimerTicks {
        khal::time::TimerTicks::from_raw(self.record_lock.waiting_since.load(Ordering::Relaxed))
    }

    /// Record that this task now holds `addr`.
    #[cfg(feature = "snapshot")]
    pub fn push_held_lock(&self, addr: usize) {
        // Find a free slot.
        for slot in &self.record_lock.held_locks {
            if slot
                .compare_exchange(0, addr, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
        debug!("held locks on task {} are full!", self.id_name());
    }

    /// Record that this task released `addr`.
    #[cfg(feature = "snapshot")]
    pub fn pop_held_lock(&self, addr: usize) {
        for slot in &self.record_lock.held_locks {
            if slot.load(Ordering::Acquire) == addr {
                slot.store(0, Ordering::Release);
                return;
            }
        }
    }

    /// Lock-free snapshot of held locks (0 entries are filtered out).
    #[cfg(feature = "snapshot")]
    pub fn held_locks_snapshot(&self) -> [usize; HELD_LOCK_SLOTS] {
        let mut out = [0usize; HELD_LOCK_SLOTS];
        for (i, slot) in self.record_lock.held_locks.iter().enumerate() {
            out[i] = slot.load(Ordering::Acquire);
        }
        out
    }
}

// private methods
impl TaskInner {
    fn new_common(identity: TaskIdentity, name: String) -> Self {
        let mut cpumask = KCpuMask::new();
        for cpu_id in 0..crate::api::active_cpu_num() {
            cpumask.set(cpu_id, true);
        }

        Self {
            identity,
            name: SpinNoIrq::new(name),
            is_init: false,
            entry: Cell::new(None),
            state: AtomicU8::new(TaskState::Ready as u8),
            // By default, the task is allowed to run on all CPUs.
            cpumask: SpinNoIrq::new(cpumask),
            cpu_id: AtomicU32::new(0),
            #[cfg(feature = "smp")]
            on_cpu: AtomicBool::new(false),
            #[cfg(feature = "smp")]
            wake_enqueue_flags: AtomicU8::new(0),
            #[cfg(feature = "smp")]
            on_cpu_mask: SpinNoIrq::new(KCpuMask::new()),
            #[cfg(feature = "preempt")]
            need_resched: AtomicBool::new(false),
            #[cfg(feature = "preempt")]
            preempt_disable_count: AtomicUsize::new(0),
            wake_sync_depth: AtomicUsize::new(0),
            interrupted: AtomicBool::new(false),
            interrupt_waker: PollSet::new(),
            workerqueue_current_work_key: AtomicUsize::new(0),
            workerqueue_current_queue_key: AtomicUsize::new(0),
            exit_code: AtomicI32::new(0),
            wait_for_exit: Completion::new(),
            kstack: None,
            ctx: TaskContextCell::new(),
            #[cfg(feature = "tls")]
            tls: TlsArea::alloc(),
            #[cfg(feature = "snapshot")]
            record_lock: PerTaskRecording::new(),
        }
    }

    /// Creates an "init task" using the current CPU states, to use as the
    /// current task.
    ///
    /// As it is the current task, no other task can switch to it until it
    /// switches out.
    ///
    /// And there is no need to set the `entry`, `kstack` or `tls` fields, as
    /// they will be filled automatically when the task is switches out.
    pub(crate) fn new_boot(name: String) -> Self {
        let mut t = Self::new_common(TaskIdentity::Internal, name);
        t.is_init = true;
        #[cfg(feature = "smp")]
        t.set_on_cpu(true);
        t
    }

    pub(crate) fn new_idle<F>(entry: F, name: String, stack_size: usize) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_with_identity(entry, name, stack_size, TaskIdentity::Idle)
    }

    pub(crate) fn new_current_idle(name: String) -> Self {
        let mut t = Self::new_common(TaskIdentity::Idle, name);
        t.is_init = true;
        #[cfg(feature = "smp")]
        t.set_on_cpu(true);
        t
    }

    pub(crate) fn into_arc(self) -> KtaskRef {
        Arc::new(KTask::new(self))
    }

    /// Returns the current state of the task.
    #[inline]
    pub fn state(&self) -> TaskState {
        self.state.load(Ordering::Acquire).into()
    }

    #[inline]
    pub(crate) fn set_state(&self, state: TaskState) {
        self.state.store(state as u8, Ordering::Release)
    }

    /// Transition the task state from `current_state` to `new_state`,
    /// Returns `true` if the current state is `current_state` and the state is successfully set to `new_state`,
    /// otherwise returns `false`.
    #[inline]
    pub(crate) fn transition_state(&self, current_state: TaskState, new_state: TaskState) -> bool {
        self.state
            .compare_exchange(
                current_state as u8,
                new_state as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[inline]
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.state(), TaskState::Running)
    }

    #[inline]
    pub(crate) fn is_ready(&self) -> bool {
        matches!(self.state(), TaskState::Ready)
    }

    #[inline]
    pub(crate) const fn is_init(&self) -> bool {
        self.is_init
    }

    #[inline]
    pub(crate) const fn is_idle(&self) -> bool {
        matches!(self.identity, TaskIdentity::Idle)
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn set_preempt_pending(&self, pending: bool) {
        self.need_resched.store(pending, Ordering::Release)
    }

    /// Enter a nested Linux-style `WF_SYNC` wake scope.
    #[inline]
    pub(crate) fn begin_wake_sync(&self) {
        self.wake_sync_depth.fetch_add(1, Ordering::Relaxed);
    }

    /// Leave a nested `WF_SYNC` wake scope.
    #[inline]
    pub(crate) fn end_wake_sync(&self) {
        let prev = self.wake_sync_depth.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "wake_sync_depth underflow");
    }

    /// Whether the current task is inside [`crate::with_wake_sync`].
    #[inline]
    pub(crate) fn is_wake_sync(&self) -> bool {
        self.wake_sync_depth.load(Ordering::Relaxed) > 0
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn can_preempt(&self, current_disable_count: usize) -> bool {
        self.preempt_disable_count.load(Ordering::Acquire) == current_disable_count
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn disable_preempt(&self) {
        self.preempt_disable_count.fetch_add(1, Ordering::Release);
    }

    #[inline]
    #[cfg(feature = "preempt")]
    pub(crate) fn enable_preempt(&self, resched: bool) {
        if self.preempt_disable_count.fetch_sub(1, Ordering::Release) == 1 && resched {
            // If current task is pending to be preempted, do rescheduling.
            Self::current_check_preempt_pending();
        }
    }

    #[cfg(feature = "preempt")]
    pub(crate) fn current_check_preempt_pending() {
        use kspin::NoPreemptIrqSave;
        let curr = crate::current();
        let need_resched = curr.need_resched.load(Ordering::Acquire);
        crate::run_queue::record_preempt_pending_check(need_resched);
        if !need_resched {
            return;
        }

        let can_preempt = curr.can_preempt(0);
        let in_exception = khal::context::in_exception_context();
        crate::run_queue::record_preempt_pending_blocked(can_preempt, in_exception);

        if can_preempt && !in_exception {
            // Note: if we want to print log msg during `preempt_resched`, we have to
            // disable preemption here, because the klogger may cause preemption.
            let mut rq = crate::current_run_queue::<NoPreemptIrqSave>();
            if curr.need_resched.load(Ordering::Acquire) && !khal::context::in_exception_context() {
                rq.preempt_resched()
            }
        }
    }

    /// Notify all tasks that join on this task.
    pub(crate) fn notify_exit(&self, exit_code: i32) {
        self.exit_code.store(exit_code, Ordering::Release);
        self.set_state(TaskState::Exited);
        self.wait_for_exit.complete_all();
    }

    #[inline]
    pub(crate) fn ctx_mut_ptr(&self) -> *mut TaskContext {
        self.ctx.as_mut_ptr()
    }

    /// Set the run-queue owner CPU.
    ///
    /// Do not call from new ktask paths. Ownership updates go through
    /// `RunQueue::{publish_task,enqueue_task,switch_to_local,set_owner_cpu}`.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_cpu_id(&self, cpu_id: LogicalCpuId) {
        self.cpu_id
            .store(cpu_id.as_usize() as u32, Ordering::Release);
    }

    /// Returns whether the task is running on a CPU.
    ///
    /// It is used to protect the task from being moved to a different run queue
    /// while it has not finished its scheduling process.
    /// The `on_cpu` field is set to `true` when the task is preparing to run on a CPU,
    /// and it is set to `false` when the task has finished its scheduling process in `clear_prev_task_on_cpu()`.
    ///
    /// SeqCst: this load and [`Self::set_on_cpu`] form one side of the wake
    /// handoff Dekker protocol with [`Self::arm_wake_enqueue`] /
    /// [`Self::take_wake_enqueue`]. Release/Acquire on two distinct atomics
    /// allows store buffering: both sides can miss each other's stores and
    /// leave a `Ready` task on no run queue.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::SeqCst)
    }

    /// Sets whether the task is running on a CPU.
    ///
    /// SeqCst: clearing this bit is the switch-out store in the wake handoff
    /// Dekker protocol; see [`Self::on_cpu`].
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_on_cpu(&self, on_cpu: bool) {
        self.on_cpu.store(on_cpu, Ordering::SeqCst)
    }

    /// Publishes an enqueue operation that may be completed by either the
    /// waker or the CPU finishing this task's switch-out.
    ///
    /// SeqCst so a later [`Self::on_cpu`] load is totally ordered with the
    /// switch-out `on_cpu=false` store and [`Self::take_wake_enqueue`] swap.
    /// At least one side then observes the other and enqueues.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn arm_wake_enqueue(&self, is_wake_sync: bool, resched: bool) {
        const PENDING: u8 = 1;
        const WAKE_SYNC: u8 = 1 << 1;
        const RESCHED: u8 = 1 << 2;

        let mut flags = PENDING;
        if is_wake_sync {
            flags |= WAKE_SYNC;
        }
        if resched {
            flags |= RESCHED;
        }
        self.wake_enqueue_flags.store(flags, Ordering::SeqCst);
    }

    /// Claims a pending wake enqueue exactly once.
    ///
    /// SeqCst swap: see [`Self::arm_wake_enqueue`].
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn take_wake_enqueue(&self) -> Option<(bool, bool)> {
        const PENDING: u8 = 1;
        const WAKE_SYNC: u8 = 1 << 1;
        const RESCHED: u8 = 1 << 2;

        let flags = self.wake_enqueue_flags.swap(0, Ordering::SeqCst);
        (flags & PENDING != 0).then_some((flags & WAKE_SYNC != 0, flags & RESCHED != 0))
    }

    /// Returns the task-local scheduler residency snapshot for this task.
    ///
    /// This state is owned by `ktask` and tracks where this task has run from
    /// the scheduler's perspective. User address-space TLB shootdown targeting
    /// is tracked separately by `memspace::MmCpuResidency`.
    #[cfg(feature = "smp")]
    #[inline]
    pub fn on_cpu_mask(&self) -> KCpuMask {
        *self.on_cpu_mask.lock()
    }

    /// Marks the given CPU in this task-local scheduler residency snapshot.
    #[cfg(feature = "smp")]
    #[inline]
    pub fn set_on_cpu_mask_bit(&self, cpu_id: LogicalCpuId) {
        self.on_cpu_mask.lock().set_logical(cpu_id, true);
    }

    /// Replaces this task-local scheduler residency snapshot with one CPU.
    #[cfg(feature = "smp")]
    #[inline]
    pub fn reset_on_cpu_mask(&self, cpu_id: LogicalCpuId) {
        let mut mask = KCpuMask::new();
        mask.set_logical(cpu_id, true);
        *self.on_cpu_mask.lock() = mask;
    }
}

impl fmt::Debug for TaskInner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut ds = f.debug_struct("TaskInner");
        ds.field("task_number", &self.task_number())
            .field("has_user_runtime", &self.user_runtime().is_some())
            .field("name", &self.name)
            .field("state", &self.state());

        #[cfg(feature = "snapshot")]
        {
            let waiting = self.waiting_lock();
            let held = self.held_locks_snapshot();
            ds.field("waiting_lock", &waiting)
                .field("held_locks", &held);
        }

        ds.finish_non_exhaustive()
    }
}

impl Drop for TaskInner {
    fn drop(&mut self) {
        debug!("task drop: {}", self.id_name());
    }
}

struct TaskStack {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl TaskStack {
    pub fn alloc(size: usize) -> Self {
        let layout = Layout::from_size_align(size, 16).unwrap();
        Self {
            // SAFETY: `layout` is validated above and the returned allocation
            // is owned exclusively by this `TaskStack`.
            ptr: NonNull::new(unsafe { alloc::alloc::alloc(layout) }).unwrap(),
            layout,
        }
    }

    pub fn top(&self) -> VirtAddr {
        // SAFETY: adding `layout.size()` to the base allocation pointer yields
        // the one-past-the-end stack-top address represented as `VirtAddr`.
        VirtAddr::from(self.ptr.as_ptr().wrapping_add(self.layout.size()) as usize)
    }
}

impl Drop for TaskStack {
    fn drop(&mut self) {
        // SAFETY: `ptr` and `layout` are the exact allocation pair created in
        // `TaskStack::alloc`, and `drop` runs exactly once for this stack.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

/// A wrapper of [`KtaskRef`] as the current task.
///
/// It won't change the reference count of the task when created or dropped.
pub struct CurrentTask(ManuallyDrop<KtaskRef>);

impl CurrentTask {
    pub(crate) fn try_get() -> Option<Self> {
        let ptr: *const super::KTask = khal::percpu::current_task_ptr();
        if !ptr.is_null() {
            // SAFETY: the percpu current-task pointer is installed from
            // `Arc::into_raw` in `init_current`/`set_current` and stays valid
            // while it is the current task pointer for this CPU.
            Some(Self(unsafe { ManuallyDrop::new(KtaskRef::from_raw(ptr)) }))
        } else {
            None
        }
    }

    pub(crate) fn get() -> Self {
        Self::try_get().expect("current task is uninitialized")
    }

    /// Clone the inner `KtaskRef`.
    #[allow(clippy::should_implement_trait)]
    pub fn clone(&self) -> KtaskRef {
        self.0.deref().clone()
    }

    /// Returns `true` if the current task is the same as `other`.
    pub fn ptr_eq(&self, other: &KtaskRef) -> bool {
        Arc::ptr_eq(&self.0, other)
    }

    pub(crate) unsafe fn init_current(init_task: KtaskRef) {
        assert!(init_task.is_init());
        #[cfg(all(feature = "tls", target_os = "none"))]
        // SAFETY: the initial task's TLS block is fully constructed before the
        // task becomes current, and this writes the current CPU's thread pointer.
        unsafe {
            khal::asm::write_thread_pointer(init_task.tls.tls_ptr() as usize)
        };
        let ptr = Arc::into_raw(init_task);
        // SAFETY: `ptr` originates from `Arc::into_raw` and becomes the owning
        // percpu current-task pointer for this CPU.
        unsafe {
            khal::percpu::set_current_task_ptr(ptr);
        }
    }

    pub(crate) unsafe fn set_current(prev: Self, next: KtaskRef) {
        let Self(arc) = prev;
        ManuallyDrop::into_inner(arc); // `call Arc::drop()` to decrease prev task reference count.
        let ptr = Arc::into_raw(next);
        // SAFETY: `ptr` originates from `Arc::into_raw` and replaces the
        // owning percpu current-task pointer for this CPU.
        unsafe {
            khal::percpu::set_current_task_ptr(ptr);
        }
    }
}

impl Deref for CurrentTask {
    type Target = KtaskRef;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

extern "C" fn task_entry() -> ! {
    #[cfg(feature = "smp")]
    // SAFETY: this runs as the first code on a scheduled task after the
    // context switch, which is exactly when the previous-task on-CPU marker
    // must be cleared for this CPU.
    unsafe {
        // Clear the prev task on CPU before running the task entry function.
        // SAFETY: a first-run task reaches this point with IRQs disabled and
        // owns its current CPU's run queue after the context switch.
        let run_queue = crate::run_queue::current_run_queue_mut();
        crate::run_queue::clear_prev_task_on_cpu(run_queue);
    }
    // Enable irq (if feature "irq" is enabled) before running the task entry function.
    karch::enable_local_irq();
    let task = crate::current();
    if let Some(entry) = task.entry.take() {
        entry()
    }
    crate::exit(0);
}

#[cfg(all(feature = "smp", unittest))]
mod tests_on_cpu_mask {
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn new_test_task() -> TaskInner {
        TaskInner::new_common(TaskIdentity::Internal, "test".into())
    }

    #[def_test]
    fn test_on_cpu_mask_initially_empty() {
        let task = new_test_task();
        assert!(task.on_cpu_mask().is_empty());
    }

    #[def_test]
    fn test_set_on_cpu_mask_bit() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = new_test_task();
            task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
            assert!(task.on_cpu_mask().get(0));
            assert!(!task.on_cpu_mask().get(1));
        }
    }

    #[def_test]
    fn test_reset_on_cpu_mask() {
        if kcpu_id_map::nr_cpus() >= 3 {
            let task = new_test_task();
            task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
            task.set_on_cpu_mask_bit(LogicalCpuId::new(1));
            task.reset_on_cpu_mask(LogicalCpuId::new(2));
            let mask = task.on_cpu_mask();
            assert!(!mask.get(0));
            assert!(!mask.get(1));
            assert!(mask.get(2));
        }
    }

    #[def_test]
    fn test_on_cpu_mask_multiple_bits() {
        if kcpu_id_map::nr_cpus() >= 4 {
            let task = new_test_task();
            task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
            task.set_on_cpu_mask_bit(LogicalCpuId::new(2));
            task.set_on_cpu_mask_bit(LogicalCpuId::new(3));
            let mask = task.on_cpu_mask();
            assert_eq!(mask.len(), 3);
            assert!(mask.get(0));
            assert!(!mask.get(1));
            assert!(mask.get(2));
            assert!(mask.get(3));
        }
    }
}
