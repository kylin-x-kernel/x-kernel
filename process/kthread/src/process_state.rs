// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec::Vec};
#[cfg(feature = "tee")]
use core::any::Any;

use kcred::Credentials;
#[cfg(feature = "tee")]
use kerrno::{KError, KResult};
use kfs::FsContext;
use kfutex::ProcessFutexState;
use khal::time::TimeValue;
use kpoll::PollSet;
use kprocess::Process;
use kresources::ProcessResources;
use ksignal::{
    Signo,
    api::{ProcessSignalManager, SignalActions},
};
use ksync::{Mutex, RwLock, spin::SpinNoIrq};
#[cfg(feature = "tee")]
use slab::Slab;
#[cfg(feature = "tee")]
use tee_task_iface::TeeTaCtx;

use crate::{
    AsThread, ProcessLifecycleState, ProcessRuntimeState, get_task, posix_state::ProcessPosixState,
};

/// Static configuration used to initialize a [`ProcessState`].
#[derive(Clone, Copy)]
pub struct ProcessStateConfig {
    /// Initial user heap base address.
    pub user_heap_base: usize,
    /// Default user stack size limit.
    pub user_stack_size: usize,
    /// Signal trampoline entry address.
    pub signal_trampoline: usize,
}

impl Default for ProcessStateConfig {
    fn default() -> Self {
        use kaddr_layout::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE};
        Self {
            user_heap_base: USER_HEAP_BASE,
            user_stack_size: USER_STACK_SIZE,
            signal_trampoline: SIGNAL_TRAMPOLINE,
        }
    }
}

/// [`Process`]-shared state.
pub struct ProcessState {
    /// The process.
    pub proc: Arc<Process>,

    /// The process-owned resource state.
    pub resources: Arc<ProcessResources>,

    /// The POSIX-facing shared state.
    posix: ProcessPosixState,

    /// The lifecycle state shared by all threads in the process.
    lifecycle: ProcessLifecycleState,

    /// The runtime state shared by all threads in the process.
    runtime: ProcessRuntimeState,

    /// The process signal manager.
    pub signal: Arc<ProcessSignalManager>,

    /// The process-owned futex state.
    futex: Arc<ProcessFutexState>,

    /// POSIX credentials shared by all threads in this process.
    pub credentials: RwLock<Credentials>,

    /// The TEE TA context.
    #[cfg(feature = "tee")]
    pub tee_ta_ctx: RwLock<TeeTaCtx>,

    /// The process-local TEE file table.
    #[cfg(feature = "tee")]
    tee_fd_table: Arc<RwLock<Slab<Arc<kfs::File>>>>,

    /// The process-local TEE private runtime state.
    #[cfg(feature = "tee")]
    tee_runtime_private: RwLock<Option<Arc<dyn Any + Send + Sync>>>,
}

impl ProcessState {
    /// Creates a new [`ProcessState`].
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        proc: Arc<Process>,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        address_space: Arc<Mutex<memspace::AddrSpace>>,
        fs_context: Arc<Mutex<FsContext>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        exit_signal: Option<Signo>,
        credentials: Credentials,
        config: ProcessStateConfig,
    ) -> Arc<Self> {
        #[cfg(feature = "tee")]
        let tee_ta_ctx = RwLock::new(TeeTaCtx::new(&exe_path));
        let posix = ProcessPosixState::new(exe_path, cmdline, exit_signal);
        let runtime =
            ProcessRuntimeState::new(proc.pid(), address_space, fs_context, config.user_heap_base);
        Arc::new(Self {
            proc,
            #[cfg(feature = "tee")]
            tee_ta_ctx,
            #[cfg(feature = "tee")]
            tee_fd_table: Arc::default(),
            #[cfg(feature = "tee")]
            tee_runtime_private: RwLock::new(None),
            resources: ProcessResources::new(config.user_stack_size),
            posix,
            lifecycle: ProcessLifecycleState::new(),
            runtime,

            signal: Arc::new(ProcessSignalManager::new(
                signal_actions,
                config.signal_trampoline,
            )),

            futex: Arc::new(ProcessFutexState::new()),
            credentials: RwLock::new(credentials),
        })
    }

    /// Returns whether this process behaves like a clone child.
    pub fn is_clone_child(&self) -> bool {
        self.exit_signal() != Some(Signo::SIGCHLD)
    }

    /// Returns the process-owned futex state.
    pub fn futex_state(&self) -> &Arc<ProcessFutexState> {
        &self.futex
    }

    /// Returns the POSIX-facing shared state.
    pub fn posix(&self) -> &ProcessPosixState {
        &self.posix
    }

    /// Returns the lifecycle state shared by all threads in the process.
    pub fn lifecycle(&self) -> &ProcessLifecycleState {
        &self.lifecycle
    }

    /// Returns the runtime state shared by all threads in the process.
    pub fn runtime(&self) -> &ProcessRuntimeState {
        &self.runtime
    }

    /// Returns the executable path.
    pub fn exe_path(&self) -> &RwLock<String> {
        self.posix.exe_path()
    }

    /// Returns the command-line arguments.
    pub fn cmdline(&self) -> &RwLock<Arc<Vec<String>>> {
        self.posix.cmdline()
    }

    /// Returns the process exit signal.
    pub fn exit_signal(&self) -> Option<Signo> {
        self.posix.exit_signal()
    }

    /// Returns the process umask.
    pub fn umask(&self) -> u32 {
        self.posix.umask()
    }

    /// Sets the process umask.
    pub fn set_umask(&self, umask: u32) {
        self.posix.set_umask(umask);
    }

    /// Sets the process umask and returns the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.posix.replace_umask(umask)
    }

    /// Returns the child-exit wait event.
    pub fn child_exit_event(&self) -> &Arc<PollSet> {
        self.lifecycle.child_exit_event()
    }

    /// Returns the process-exit event.
    pub fn exit_event(&self) -> &Arc<PollSet> {
        self.lifecycle.exit_event()
    }

    /// Returns the virtual address space.
    pub fn address_space(&self) -> &Arc<Mutex<memspace::AddrSpace>> {
        self.runtime.address_space()
    }

    /// Returns the process-owned filesystem context.
    pub fn fs_context(&self) -> &Arc<Mutex<FsContext>> {
        self.runtime.fs_context()
    }

    /// Returns the top address of the user heap.
    pub fn heap_top(&self) -> usize {
        self.runtime.heap_top()
    }

    /// Sets the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.runtime.set_heap_top(top);
    }

    /// Returns the process-owned timer manager.
    pub fn timer_manager(&self) -> &Arc<Mutex<ktimer::ProcessTimerManager>> {
        self.runtime.timer_manager()
    }

    /// Samples process user and system CPU time in nanoseconds.
    pub fn process_cpu_time_ns(&self) -> (usize, usize) {
        self.proc
            .threads()
            .into_iter()
            .fold((0, 0), |(utime_ns, stime_ns), tid| {
                if let Ok(task) = get_task(tid) {
                    let (thread_utime_ns, thread_stime_ns) = task.as_thread().sample_cpu_time_ns();
                    (
                        utime_ns.saturating_add(thread_utime_ns),
                        stime_ns.saturating_add(thread_stime_ns),
                    )
                } else {
                    (utime_ns, stime_ns)
                }
            })
    }

    /// Returns the process user and system CPU time.
    pub fn process_cpu_times(&self) -> (TimeValue, TimeValue) {
        let (utime_ns, stime_ns) = self.process_cpu_time_ns();
        (
            TimeValue::from_nanos(utime_ns as u64),
            TimeValue::from_nanos(stime_ns as u64),
        )
    }

    /// Returns the total process CPU time (user + system).
    pub fn process_cpu_time(&self) -> TimeValue {
        let (utime, stime) = self.process_cpu_times();
        utime + stime
    }

    /// Adds reaped-child CPU time to the accumulated counters.
    pub fn accumulate_child_time(&self, utime_ns: usize, stime_ns: usize) {
        self.lifecycle.accumulate_child_time(utime_ns, stime_ns);
    }

    /// Returns accumulated reaped-children user and kernel time in nanoseconds.
    pub fn child_time_ns(&self) -> (usize, usize) {
        self.lifecycle.child_time_ns()
    }

    /// Returns the process-local TEE file table.
    #[cfg(feature = "tee")]
    pub fn tee_fd_table(&self) -> &Arc<RwLock<Slab<Arc<kfs::File>>>> {
        &self.tee_fd_table
    }

    /// Gets the process-local TEE private state if it has been initialized.
    #[cfg(feature = "tee")]
    pub fn tee_runtime_private<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync,
    {
        let state = self.tee_runtime_private.read().clone()?;
        state.downcast::<T>().ok()
    }

    /// Gets or initializes the process-local TEE private state.
    #[cfg(feature = "tee")]
    pub fn get_or_init_tee_runtime_private<T, F>(&self, init_fn: F) -> KResult<Arc<T>>
    where
        T: Any + Send + Sync,
        F: FnOnce() -> T,
    {
        if let Some(state) = self.tee_runtime_private::<T>() {
            return Ok(state);
        }

        let state = Arc::new(init_fn());
        let erased: Arc<dyn Any + Send + Sync> = state.clone();
        let mut slot = self.tee_runtime_private.write();
        if let Some(existing) = slot.as_ref() {
            return existing
                .clone()
                .downcast::<T>()
                .map_err(|_| KError::from(kerrno::KErrorKind::BadState));
        }

        *slot = Some(erased);
        Ok(state)
    }

    /// Clears the process-local TEE private state.
    #[cfg(feature = "tee")]
    pub fn clear_tee_runtime_private(&self) {
        *self.tee_runtime_private.write() = None;
    }
}
