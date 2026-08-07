// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod posix_state;
mod runtime_state;

use alloc::{string::String, sync::Arc, vec::Vec};
#[cfg(feature = "tee")]
use core::any::Any;
use core::sync::atomic::AtomicUsize;

use fs_context::FsStruct;
use kerrno::{KError, KResult};
use kns::{NamespaceFsContext, NsProxy};
use kresources::ProcessResources;
use ksignal::{
    MAX_SIGNALS, SignalDisposition, Signo,
    api::{ProcessSignalManager, SignalActions},
};
use ksync::{Mutex, RwLock, spin::SpinNoIrq};
use memspace::process_lifetime::MmUserHandle;
#[cfg(feature = "tee")]
use tee_task_iface::TeeTaCtx;
#[cfg(feature = "tipc")]
use tipc_handle::HandleTable as TipcHandleTable;

use self::{
    posix_state::{ExecMetadata, ProcessPosixState},
    runtime_state::ProcessRuntimeState,
};
use crate::{LiveAddressSpace, Process, process_domain};

/// Static configuration used to initialize a [`ProcessRuntime`].
#[derive(Clone, Copy)]
pub(crate) struct ProcessRuntimeConfig {
    /// Initial user heap base address.
    pub user_heap_base: usize,
    /// Default user stack size limit.
    pub user_stack_size: usize,
    /// Signal trampoline entry address.
    pub signal_trampoline: usize,
}

impl Default for ProcessRuntimeConfig {
    fn default() -> Self {
        use kaddr_layout::{SIGNAL_TRAMPOLINE, USER_HEAP_BASE, USER_STACK_SIZE};
        Self {
            user_heap_base: USER_HEAP_BASE,
            user_stack_size: USER_STACK_SIZE,
            signal_trampoline: SIGNAL_TRAMPOLINE,
        }
    }
}

/// Fork-time options for constructing a child process.
#[derive(Clone, Copy)]
pub struct ProcessForkConfig {
    /// Wait-parent selection for the new process.
    pub parent: ForkParent,
    /// Address-space clone mode.
    pub address_space: ForkAddressSpace,
    /// Filesystem-context clone mode.
    pub fs: ForkFs,
    /// Signal-action clone mode.
    pub signal_actions: ForkSignalActions,
    /// File-descriptor table clone mode.
    pub fd_table: ForkFdTable,
    /// Namespace flags requested by the clone/fork caller.
    pub namespace_flags: kns::NamespaceFlags,
    /// Exit signal configured for the new process.
    pub exit_signal: Option<Signo>,
}

/// Wait-parent selection for a forked process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkParent {
    /// Parent the child under the caller.
    Caller,
    /// Parent the child under the caller's current wait parent.
    CallerParent,
}

/// Address-space clone mode for a forked process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkAddressSpace {
    /// Clone the caller's address space.
    Private,
    /// Share the caller's address space.
    Shared,
}

/// Filesystem-context clone mode for a forked process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkFs {
    /// Clone the caller's filesystem context.
    Private,
    /// Share the caller's filesystem context.
    Shared,
}

/// Signal-action clone mode for a forked process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkSignalActions {
    /// Clone the caller's signal actions.
    Private,
    /// Share the caller's signal actions.
    Shared,
}

/// File-descriptor table clone mode for a forked process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForkFdTable {
    /// Clone the caller's file-descriptor table.
    Private,
    /// Share the caller's file-descriptor table.
    Shared,
}

/// Runtime state shared by all threads in a process.
pub(crate) struct ProcessRuntime {
    process: Arc<Process>,
    resources: Arc<ProcessResources>,
    fs_context: Arc<Mutex<FsStruct>>,
    posix_state: ProcessPosixState,
    runtime_state: ProcessRuntimeState,
    nsproxy: RwLock<Arc<NsProxy>>,
    signal_manager: Arc<ProcessSignalManager>,
    #[cfg(feature = "tee")]
    tee_ta_ctx: RwLock<TeeTaCtx>,
    #[cfg(feature = "tee")]
    tee_runtime_private: RwLock<Option<Arc<dyn Any + Send + Sync>>>,
    #[cfg(feature = "tipc")]
    tipc_handles: RwLock<TipcHandleTable>,
}

/// RAII rollback guard for a child process attached during fork preparation.
///
/// If fork fails before publication commits, dropping this guard removes the
/// unpublished child relation so no hidden process object remains linked.
struct AttachedForkProcess {
    process: Option<Arc<Process>>,
}

struct ForkFsNamespaces {
    fs_context: Arc<Mutex<FsStruct>>,
    nsproxy: Arc<NsProxy>,
}

struct ForkAddressSpaceState {
    mm_user: MmUserHandle,
}

impl AttachedForkProcess {
    fn new(process: Arc<Process>) -> Self {
        Self {
            process: Some(process),
        }
    }

    fn into_process(mut self) -> Arc<Process> {
        self.process
            .take()
            .expect("attached fork process must be present")
    }
}

impl Drop for AttachedForkProcess {
    fn drop(&mut self) {
        if let Some(process) = self.process.take() {
            let domain = process_domain::write_lock();
            process.discard_unpublished_locked(&domain);
        }
    }
}

impl ProcessRuntime {
    /// Creates a new [`ProcessRuntime`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        process: Arc<Process>,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        address_space: Arc<Mutex<memspace::MmSpace>>,
        fs_context: Arc<Mutex<FsStruct>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        config: ProcessRuntimeConfig,
    ) -> Arc<Self> {
        let nsproxy = NsProxy::new_initial();
        Self::new_with_nsproxy(
            process,
            exe_path,
            cmdline,
            address_space,
            fs_context,
            signal_actions,
            config,
            nsproxy,
        )
    }

    /// Creates a new [`ProcessRuntime`] with an explicit [`NsProxy`].
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_nsproxy(
        process: Arc<Process>,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        address_space: Arc<Mutex<memspace::MmSpace>>,
        fs_context: Arc<Mutex<FsStruct>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        config: ProcessRuntimeConfig,
        nsproxy: Arc<NsProxy>,
    ) -> Arc<Self> {
        let mm_user = memspace::MmSpace::acquire_user(address_space)
            .expect("new process runtime requires a live user address space");
        Self::new_with_nsproxy_and_mm_user(
            process,
            exe_path,
            cmdline,
            mm_user,
            fs_context,
            signal_actions,
            config,
            nsproxy,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_nsproxy_and_mm_user(
        process: Arc<Process>,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        mm_user: MmUserHandle,
        fs_context: Arc<Mutex<FsStruct>>,
        signal_actions: Arc<SpinNoIrq<SignalActions>>,
        config: ProcessRuntimeConfig,
        nsproxy: Arc<NsProxy>,
    ) -> Arc<Self> {
        #[cfg(feature = "tee")]
        let tee_ta_ctx = RwLock::new(TeeTaCtx::new(&exe_path));
        let posix_state = ProcessPosixState::new(exe_path, cmdline);
        let runtime_state = ProcessRuntimeState::new(process.pid(), mm_user, config.user_heap_base);
        let runtime = Arc::new(Self {
            process,
            #[cfg(feature = "tee")]
            tee_ta_ctx,
            #[cfg(feature = "tee")]
            tee_runtime_private: RwLock::new(None),
            #[cfg(feature = "tipc")]
            tipc_handles: RwLock::new(TipcHandleTable::new()),
            resources: ProcessResources::new(config.user_stack_size),
            fs_context,
            posix_state,
            runtime_state,
            nsproxy: RwLock::new(nsproxy),
            signal_manager: Arc::new(ProcessSignalManager::new(
                signal_actions,
                config.signal_trampoline,
            )),
        });
        runtime.process.set_runtime_ref(&runtime);
        runtime
    }

    /// Returns the stable process identity that owns this runtime.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns the process-owned resource state.
    pub fn resources(&self) -> &Arc<ProcessResources> {
        &self.resources
    }

    /// Returns the live process signal manager.
    pub fn signal_manager(&self) -> &Arc<ProcessSignalManager> {
        &self.signal_manager
    }

    /// Runs a closure with mutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx_mut<R>(&self, f: impl FnOnce(&mut TeeTaCtx) -> R) -> R {
        let mut tee_ta_ctx = self.tee_ta_ctx.write();
        f(&mut tee_ta_ctx)
    }

    /// Runs a closure with immutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx<R>(&self, f: impl FnOnce(&TeeTaCtx) -> R) -> R {
        let tee_ta_ctx = self.tee_ta_ctx.read();
        f(&tee_ta_ctx)
    }

    /// Runs a closure with access to the process-local Trusty IPC handle table.
    #[cfg(feature = "tipc")]
    pub(crate) fn with_tipc_handles<R>(&self, f: impl FnOnce(&RwLock<TipcHandleTable>) -> R) -> R {
        f(&self.tipc_handles)
    }

    /// Returns the executable metadata snapshot.
    fn exec_metadata(&self) -> ExecMetadata {
        self.posix_state.exec_metadata()
    }

    /// Returns the executable path.
    pub fn exe_path(&self) -> String {
        String::from(self.exec_metadata().exe_path())
    }

    /// Returns the command-line arguments.
    pub fn cmdline(&self) -> Arc<Vec<String>> {
        self.exec_metadata().cmdline().clone()
    }

    /// Returns the process OOM score adjustment.
    pub fn oom_score_adj(&self) -> i32 {
        self.posix_state.oom_score_adj()
    }

    /// Sets the process OOM score adjustment.
    pub fn set_oom_score_adj(&self, value: i32) {
        self.posix_state.set_oom_score_adj(value);
    }

    /// Updates the executable metadata snapshot after a successful exec.
    pub fn set_exec_metadata(&self, exe_path: String, cmdline: Arc<Vec<String>>) {
        self.posix_state.set_exec_metadata(exe_path, cmdline);
    }

    /// Returns a live address-space capability if user mappings are still active.
    pub(crate) fn address_space(&self) -> Option<LiveAddressSpace> {
        self.runtime_state
            .clone_mm_user()
            .map(LiveAddressSpace::new)
    }

    /// Returns the pinned address-space object for teardown-state observation.
    #[cfg(unittest)]
    pub(crate) fn pinned_address_space_for_teardown_observation(
        &self,
    ) -> &Arc<Mutex<memspace::MmSpace>> {
        self.runtime_state.pinned_address_space()
    }

    /// Returns the immutable address-space identity used by private futex keys.
    pub fn mm_id(&self) -> u64 {
        self.runtime_state.mm_id()
    }

    pub(crate) fn clear_exclusive_address_space(&self) -> bool {
        self.runtime_state.clear_exclusive_address_space()
    }

    /// Returns the process-owned filesystem context.
    pub fn fs_context(&self) -> Arc<Mutex<FsStruct>> {
        self.fs_context.clone()
    }

    /// Returns the namespace proxy.
    pub fn nsproxy(&self) -> Arc<NsProxy> {
        self.nsproxy.read().clone()
    }

    /// Returns the UTS namespace.
    pub fn uts_ns(&self) -> Arc<kns::UtsNamespace> {
        self.nsproxy.read().uts_ns().clone()
    }

    /// Returns the top address of the user heap.
    pub fn heap_top(&self) -> usize {
        self.runtime_state.heap_top()
    }

    /// Returns a shared handle to the heap top (live while the process runs).
    pub fn heap_top_handle(&self) -> Arc<AtomicUsize> {
        self.runtime_state.heap_top_handle()
    }

    /// Sets the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.runtime_state.set_heap_top(top);
    }

    /// Returns the process-owned timer manager.
    pub(crate) fn timer_manager(&self) -> &Arc<Mutex<ktimer::ProcessTimerManager>> {
        self.runtime_state.timer_manager()
    }

    /// Records the CPU that currently owns this process mm residency.
    pub fn set_mm_resident_cpu(&self, cpu_id: kcpu_id_map::LogicalCpuId) {
        self.runtime_state.mm_cpu_residency().set_cpu(cpu_id);
    }

    /// Returns the hardware page-table root for this process.
    #[cfg(target_arch = "aarch64")]
    pub fn page_table_hw_root(&self) -> karch::HwPageTableRoot {
        self.runtime_state.page_table_hw_root()
    }

    /// Clears the process-local TEE private state.
    #[cfg(feature = "tee")]
    pub fn clear_tee_runtime_private(&self) {
        *self.tee_runtime_private.write() = None;
    }

    /// Resets process-wide signal actions after exec.
    ///
    /// Linux preserves handlers explicitly set to `SIG_IGN` across exec, while
    /// caught handlers are reset to `SIG_DFL` and their flags/masks/restorer are
    /// cleared.
    pub fn reset_signal_actions(&self) {
        let mut actions = self.signal_manager.actions.lock();
        for raw in 1..=MAX_SIGNALS as u8 {
            let signo = Signo::from_repr(raw).expect("valid signal number");
            if !matches!(actions[signo].disposition, SignalDisposition::Ignore) {
                actions[signo] = Default::default();
            }
        }
    }

    /// Clears all process POSIX timers.
    pub fn clear_posix_timers(&self) {
        self.timer_manager().lock().clear_posix_timers();
    }
}

/// Creates a child process runtime for fork/clone process creation paths.
#[doc(hidden)]
pub(crate) fn fork_process_runtime(
    parent: &Arc<ProcessRuntime>,
    config: ProcessForkConfig,
) -> KResult<(Arc<ProcessRuntime>, Arc<kidentity::PidHandle>)> {
    let fs_namespaces = prepare_fork_fs_and_namespaces(parent, &config)?;
    let leader_task_number = kidentity::allocate_root_pid_handle()?;
    let process = parent.process().fork_with_tree_parent(
        leader_task_number.clone(),
        config.parent,
        config.exit_signal,
    )?;
    let attached_process = AttachedForkProcess::new(process);

    let address_space = prepare_fork_address_space(parent, config.address_space)?;
    let signal_actions = prepare_fork_signal_actions(parent, config.signal_actions);
    let process = attached_process.into_process();
    let process_runtime = finish_fork_runtime(
        parent,
        process,
        fs_namespaces,
        address_space,
        signal_actions,
        config,
    );

    Ok((process_runtime, leader_task_number))
}

fn prepare_fork_fs_and_namespaces(
    parent: &Arc<ProcessRuntime>,
    config: &ProcessForkConfig,
) -> KResult<ForkFsNamespaces> {
    let parent_fs_context = parent.fs_context();
    let fs_context = if matches!(config.fs, ForkFs::Shared) {
        if parent_fs_context.lock().in_exec() {
            return Err(KError::WouldBlock);
        }
        parent_fs_context
    } else {
        Arc::new(Mutex::new(parent_fs_context.lock().clone_for_process()))
    };

    let nsproxy_result = if matches!(config.fs, ForkFs::Shared) {
        parent
            .nsproxy()
            .clone_for_child(config.namespace_flags, NamespaceFsContext::Shared)
    } else {
        let mut fs = fs_context.lock();
        parent
            .nsproxy()
            .clone_for_child(config.namespace_flags, NamespaceFsContext::Private(&mut fs))
    };
    let nsproxy = nsproxy_result.map_err(|err| match err {
        kns::CloneNsError::InvalidFlagCombination => KError::InvalidInput,
        kns::CloneNsError::Unimplemented => KError::Unsupported,
        kns::CloneNsError::Mount(err) => err,
    })?;

    Ok(ForkFsNamespaces {
        fs_context,
        nsproxy,
    })
}

fn prepare_fork_address_space(
    parent: &Arc<ProcessRuntime>,
    mode: ForkAddressSpace,
) -> KResult<ForkAddressSpaceState> {
    let parent_mm_user = parent
        .runtime_state
        .clone_mm_user()
        .ok_or(KError::NoSuchProcess)?;
    let mm_user = if matches!(mode, ForkAddressSpace::Shared) {
        parent_mm_user
    } else {
        let mut aspace = parent_mm_user.address_space().lock();
        let address_space = aspace.try_clone()?;
        memspace::MmSpace::acquire_user(address_space).ok_or(KError::NoSuchProcess)?
    };

    Ok(ForkAddressSpaceState { mm_user })
}

fn prepare_fork_signal_actions(
    parent: &Arc<ProcessRuntime>,
    mode: ForkSignalActions,
) -> Arc<SpinNoIrq<SignalActions>> {
    if matches!(mode, ForkSignalActions::Shared) {
        parent.signal_manager().actions.clone()
    } else {
        Arc::new(SpinNoIrq::new(
            parent.signal_manager().actions.lock().clone(),
        ))
    }
}

fn finish_fork_runtime(
    parent: &Arc<ProcessRuntime>,
    process: Arc<Process>,
    fs_namespaces: ForkFsNamespaces,
    address_space: ForkAddressSpaceState,
    signal_actions: Arc<SpinNoIrq<SignalActions>>,
    config: ProcessForkConfig,
) -> Arc<ProcessRuntime> {
    let exec_metadata = parent.exec_metadata();
    let process_runtime = ProcessRuntime::new_with_nsproxy_and_mm_user(
        process,
        String::from(exec_metadata.exe_path()),
        exec_metadata.cmdline().clone(),
        address_space.mm_user,
        fs_namespaces.fs_context,
        signal_actions,
        ProcessRuntimeConfig::default(),
        fs_namespaces.nsproxy,
    );
    process_runtime.set_oom_score_adj(parent.oom_score_adj());
    process_runtime.set_heap_top(parent.heap_top());

    if matches!(config.fd_table, ForkFdTable::Shared) {
        process_runtime
            .resources()
            .replace_fd_table(parent.resources().fd_table());
    } else {
        let fd_table = kfd::FdTable::clone_shared_from(&parent.resources().fd_table());
        process_runtime.resources().replace_fd_table(fd_table);
    }

    process_runtime
}
