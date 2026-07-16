// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core task data structures and lifecycle helpers.

use alloc::{boxed::Box, string::String, sync::Arc};
#[cfg(feature = "preempt")]
use core::sync::atomic::AtomicUsize;
use core::{
    alloc::Layout,
    cell::{Cell, UnsafeCell},
    fmt,
    future::poll_fn,
    mem::ManuallyDrop,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering},
    task::{Context, Poll},
};

use futures_util::task::AtomicWaker;
#[cfg(feature = "smp")]
use kcpu_id_map::KCpuMaskExt;
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use khal::context::TaskContext;
#[cfg(feature = "tls")]
use khal::tls::TlsArea;
use kspin::SpinNoIrq;
use memaddr::{VirtAddr, align_up_4k};

use crate::{KCpuMask, KTask, KtaskRef, future::block_on};

#[derive(Debug, Clone)]
enum TaskIdentity {
    Idle,
    Internal,
    Thread(Arc<kidentity::PidHandle>),
}

impl TaskIdentity {
    fn thread_pid(&self) -> Option<&Arc<kidentity::PidHandle>> {
        match self {
            Self::Thread(task_number) => Some(task_number),
            Self::Idle | Self::Internal => None,
        }
    }

    fn trace_id(&self) -> u64 {
        match self {
            Self::Idle => 0,
            Self::Internal => 0,
            Self::Thread(task_number) => task_number.root_nr() as u64,
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

/// User-defined task extended data.
/// # Safety
/// See [`extern_trait`].
#[extern_trait::extern_trait(
    /// The impl proxy type for [`TaskExt`].
    pub KTaskExt
)]
pub unsafe trait TaskExt {
    /// Called when the task is switched in.
    fn on_enter(&self) {}
    /// Called when the task is switched out.
    fn on_leave(&self) {}
    /// Marks that the current CPU may retain TLB state for this task's user
    /// address space.
    fn set_user_mm_resident_cpu(&self, _cpu_id: LogicalCpuId) {}
    /// Returns the latest hardware user page-table root for switch-in.
    fn switch_page_table_root(&self) -> Option<karch::HwPageTableRoot> {
        None
    }
}

// How many held locks we track per task (debug only).
#[cfg(feature = "snapshot")]
const HELD_LOCK_SLOTS: usize = 4;
#[cfg(feature = "snapshot")]
type HeldLocks = [AtomicUsize; HELD_LOCK_SLOTS];

#[cfg(feature = "snapshot")]
struct PerTaskRecording {
    /// 0 = not waiting, otherwise lock address.
    waiting_lock: AtomicUsize,
    /// Tick timestamp when we started waiting on `waiting_lock`.
    waiting_since: AtomicUsize,
    held_locks: HeldLocks,
}

#[cfg(feature = "snapshot")]
impl PerTaskRecording {
    fn new() -> Self {
        Self {
            waiting_lock: AtomicUsize::new(0),
            waiting_since: AtomicUsize::new(0),
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

    /// Bitmask of CPUs this task has been scheduled on since the last TLB
    /// shootdown. Used to limit the scope of cross-CPU TLB invalidation.
    #[cfg(feature = "smp")]
    on_cpu_mask: SpinNoIrq<KCpuMask>,

    #[cfg(feature = "preempt")]
    need_resched: AtomicBool,
    #[cfg(feature = "preempt")]
    preempt_disable_count: AtomicUsize,

    interrupted: AtomicBool,
    interrupt_waker: AtomicWaker,

    exit_code: AtomicI32,
    wait_for_exit: AtomicWaker,

    kstack: Option<TaskStack>,
    ctx: TaskContextCell,

    task_ext: Option<KTaskExt>,

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
            TaskIdentity::Thread(kidentity::allocate_root_pid_handle()?),
        ))
    }

    /// Creates a user thread with a preallocated thread identity.
    ///
    /// Callers must allocate the thread identity in the correct PID namespace
    /// before constructing the task, so process/thread-group/publication state
    /// can be built around the same handle before the task becomes runnable.
    pub fn new_user<F>(
        entry: F,
        name: String,
        stack_size: usize,
        task_number: Arc<kidentity::PidHandle>,
    ) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_with_identity(entry, name, stack_size, TaskIdentity::Thread(task_number))
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
        block_on(poll_fn(|cx| {
            if self.state() == TaskState::Exited {
                return Poll::Ready(self.exit_code.load(Ordering::Acquire));
            }
            self.wait_for_exit.register(cx.waker());
            Poll::Pending
        }))
    }

    /// Returns a reference to the task extended data.
    pub fn task_ext(&self) -> Option<&KTaskExt> {
        self.task_ext.as_ref()
    }

    /// Returns a mutable reference to the task extended data.
    pub fn task_ext_mut(&mut self) -> &mut Option<KTaskExt> {
        &mut self.task_ext
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

    /// Returns the CPU ID where the task is running or will run.
    ///
    /// Note: the task may not be running on the CPU, it just exists in the run queue.
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
    pub fn poll_interrupt(&self, cx: &Context) -> Poll<()> {
        if self.interrupted.swap(false, Ordering::AcqRel) {
            Poll::Ready(())
        } else {
            self.interrupt_waker.register(cx.waker());
            Poll::Pending
        }
    }

    /// Clears the interrupt state of the task.
    #[inline]
    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::Release);
    }

    /// Interrupts the task.
    #[inline]
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        self.interrupt_waker.wake();
    }

    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn set_waiting_lock(&self, lock: usize, now: usize) {
        // Publish `since` first, then `lock` with Release so readers that see
        // a non-zero lock also see the matching `since`.
        self.record_lock.waiting_since.store(now, Ordering::Relaxed);
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
    pub fn waiting_snapshot(&self) -> Option<(usize, usize)> {
        let lock = self.record_lock.waiting_lock.load(Ordering::Acquire);
        if lock == 0 {
            return None;
        }
        // Since lock is observed with Acquire and stored with Release, this
        // relaxed load is ordered after the lock read and should see the
        // corresponding `since` in practice.
        let since = self.record_lock.waiting_since.load(Ordering::Relaxed);
        if since == 0 {
            None
        } else {
            Some((lock, since))
        }
    }

    /// Getter: current waiting lock address (0 means none).
    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn waiting_lock(&self) -> usize {
        self.record_lock.waiting_lock.load(Ordering::Acquire)
    }

    /// Getter: tick when waiting started (0 means none).
    #[cfg(feature = "snapshot")]
    #[inline(always)]
    pub fn waiting_since(&self) -> usize {
        self.record_lock.waiting_since.load(Ordering::Relaxed)
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
            on_cpu_mask: SpinNoIrq::new(KCpuMask::new()),
            #[cfg(feature = "preempt")]
            need_resched: AtomicBool::new(false),
            #[cfg(feature = "preempt")]
            preempt_disable_count: AtomicUsize::new(0),
            interrupted: AtomicBool::new(false),
            interrupt_waker: AtomicWaker::new(),
            exit_code: AtomicI32::new(0),
            wait_for_exit: AtomicWaker::new(),
            kstack: None,
            ctx: TaskContextCell::new(),
            task_ext: None,
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
    fn current_check_preempt_pending() {
        use kspin::NoPreemptIrqSave;
        let curr = crate::current();
        if curr.need_resched.load(Ordering::Acquire)
            && curr.can_preempt(0)
            && !khal::context::in_exception_context()
        {
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
        self.set_state(TaskState::Exited);
        self.exit_code.store(exit_code, Ordering::Release);
        self.wait_for_exit.wake();
    }

    #[inline]
    pub(crate) fn ctx_mut_ptr(&self) -> *mut TaskContext {
        self.ctx.as_mut_ptr()
    }

    /// Set the CPU ID where the task is running or will run.
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
    /// The `on_cpu field is set to `true` when the task is preparing to run on a CPU,
    /// and it is set to `false` when the task has finished its scheduling process in `clear_prev_task_on_cpu()`.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// Sets whether the task is running on a CPU.
    #[cfg(feature = "smp")]
    #[inline]
    pub(crate) fn set_on_cpu(&self, on_cpu: bool) {
        self.on_cpu.store(on_cpu, Ordering::Release)
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
        ds.field("identity", &self.identity)
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
        crate::run_queue::clear_prev_task_on_cpu();
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
