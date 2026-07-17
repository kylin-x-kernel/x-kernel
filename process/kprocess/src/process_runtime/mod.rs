// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod posix_state;
mod runtime_state;

use alloc::{string::String, sync::Arc, vec::Vec};
#[cfg(feature = "tee")]
use core::any::Any;

use fs_context::FsStruct;
use kcred::Credentials;
use kerrno::{KError, KResult};
use kns::{NamespaceFsContext, NsProxy};
use kresources::ProcessResources;
use ksignal::{
    MAX_SIGNALS, SignalDisposition, Signo,
    api::{ProcessSignalManager, SignalActions},
};
use ksync::{Mutex, RwLock, spin::SpinNoIrq};
#[cfg(feature = "tee")]
use tee_task_iface::TeeTaCtx;
#[cfg(feature = "tipc")]
use tipc_handle::HandleTable as TipcHandleTable;

use self::{posix_state::ProcessPosixState, runtime_state::ProcessRuntimeState};
use crate::Process;

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
    /// Whether to reparent the new child under the caller's parent.
    pub share_parent: bool,
    /// Whether to share the caller's address space.
    pub share_vm: bool,
    /// Whether to share the caller's filesystem context.
    pub share_fs: bool,
    /// Whether to share process signal actions.
    pub share_sighand: bool,
    /// Whether to share the caller's file-descriptor table.
    pub share_files: bool,
    /// Namespace flags requested by the clone/fork caller.
    pub namespace_flags: kns::NamespaceFlags,
    /// Exit signal configured for the new process.
    pub exit_signal: Option<Signo>,
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
    credentials: RwLock<Credentials>,
    #[cfg(feature = "tee")]
    tee_ta_ctx: RwLock<TeeTaCtx>,
    #[cfg(feature = "tee")]
    tee_runtime_private: RwLock<Option<Arc<dyn Any + Send + Sync>>>,
    #[cfg(feature = "tipc")]
    tipc_handles: RwLock<TipcHandleTable>,
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
        credentials: Credentials,
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
            credentials,
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
        credentials: Credentials,
        config: ProcessRuntimeConfig,
        nsproxy: Arc<NsProxy>,
    ) -> Arc<Self> {
        #[cfg(feature = "tee")]
        let tee_ta_ctx = RwLock::new(TeeTaCtx::new(&exe_path));
        let posix_state = ProcessPosixState::new(exe_path, cmdline);
        let runtime_state =
            ProcessRuntimeState::new(process.pid(), address_space, config.user_heap_base);
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
            credentials: RwLock::new(credentials),
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

    /// Runs a closure with read-only access to process-shared credentials.
    pub fn with_credentials<R>(&self, f: impl FnOnce(&Credentials) -> R) -> R {
        let credentials = self.credentials.read();
        f(&credentials)
    }

    /// Runs a closure with mutable access to process-shared credentials.
    pub fn with_credentials_mut<R>(&self, f: impl FnOnce(&mut Credentials) -> R) -> R {
        let mut credentials = self.credentials.write();
        f(&mut credentials)
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

    /// Returns the executable path.
    pub fn exe_path(&self) -> &RwLock<String> {
        self.posix_state.exe_path()
    }

    /// Returns the command-line arguments.
    pub fn cmdline(&self) -> &RwLock<Arc<Vec<String>>> {
        self.posix_state.cmdline()
    }

    /// Returns the process umask.
    pub fn umask(&self) -> u32 {
        self.posix_state.umask()
    }

    /// Sets the process umask.
    pub fn set_umask(&self, umask: u32) {
        self.posix_state.set_umask(umask);
    }

    /// Sets the process umask and returns the old value.
    pub fn replace_umask(&self, umask: u32) -> u32 {
        self.posix_state.replace_umask(umask)
    }

    /// Updates the executable metadata snapshot after a successful exec.
    pub fn set_exec_metadata(&self, exe_path: String, cmdline: Arc<Vec<String>>) {
        *self.exe_path().write() = exe_path;
        *self.cmdline().write() = cmdline;
    }

    /// Returns the virtual address space.
    pub fn address_space(&self) -> &Arc<Mutex<memspace::MmSpace>> {
        self.runtime_state.address_space()
    }

    /// Returns the immutable address-space identity used by private futex keys.
    pub fn mm_id(&self) -> u64 {
        self.runtime_state.mm_id()
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

    /// Sets the top address of the user heap.
    pub fn set_heap_top(&self, top: usize) {
        self.runtime_state.set_heap_top(top);
    }

    /// Returns the process-owned timer manager.
    pub fn timer_manager(&self) -> &Arc<Mutex<ktimer::ProcessTimerManager>> {
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
    let parent_fs_context = parent.fs_context();
    let fs_context = if config.share_fs {
        if parent_fs_context.lock().in_exec() {
            return Err(KError::WouldBlock);
        }
        parent_fs_context
    } else {
        Arc::new(Mutex::new(parent_fs_context.lock().clone_for_process()))
    };

    let nsproxy_result = if config.share_fs {
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
    let leader_task_number = kidentity::allocate_root_pid_handle()?;
    let parent_process = if config.share_parent {
        parent.process().parent().ok_or(KError::InvalidInput)?
    } else {
        parent.process().clone()
    };
    let process =
        parent_process.fork_with_task_number(leader_task_number.clone(), config.exit_signal);

    let address_space = if config.share_vm {
        parent.address_space().clone()
    } else {
        let mut aspace = parent.address_space().lock();
        aspace.try_clone()?
    };
    let signal_actions = if config.share_sighand {
        parent.signal_manager().actions.clone()
    } else {
        Arc::new(SpinNoIrq::new(
            parent.signal_manager().actions.lock().clone(),
        ))
    };
    let process_runtime = ProcessRuntime::new_with_nsproxy(
        process,
        parent.exe_path().read().clone(),
        parent.cmdline().read().clone(),
        address_space,
        fs_context,
        signal_actions,
        parent.with_credentials(Clone::clone),
        ProcessRuntimeConfig::default(),
        nsproxy,
    );
    process_runtime.set_umask(parent.umask());
    process_runtime.set_heap_top(parent.heap_top());

    if config.share_files {
        process_runtime
            .resources()
            .replace_fd_table(parent.resources().fd_table());
    } else {
        let fd_table = kfd::FdTable::clone_shared_from(&parent.resources().fd_table());
        process_runtime.resources().replace_fd_table(fd_table);
    }

    Ok((process_runtime, leader_task_number))
}
