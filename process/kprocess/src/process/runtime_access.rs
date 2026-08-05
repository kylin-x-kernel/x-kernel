// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};

use fs_context::FsStruct;
use kcred::Cred;
use kerrno::{KError, KResult};
use khal::paging::MappingFlags;
use ksignal::api::{ProcessSignalManager, SignalActions};
use ksync::{Mutex, MutexGuard, spin::SpinNoIrq as KSyncSpinNoIrq};
use ktime_types::TimeSpan;
use ktimer::{PosixTimerCreateNotify, TimerDelivery};
use memaddr::VirtAddr;
use memspace::{
    InvalidateHandle, MmObserver, MmSpace, MremapSource, process_lifetime::MmUserHandle,
};
use posix_types::ITimerType;

use super::Process;
use crate::{AsThread, ProcessRuntime};

/// Live address-space capability.
///
/// Holding this value keeps the process mm user alive, so the returned
/// `MmSpace` cannot be torn down by process exit until the capability is
/// dropped.
pub struct LiveAddressSpace {
    mm_user: MmUserHandle,
}

impl LiveAddressSpace {
    pub(crate) fn new(mm_user: MmUserHandle) -> Self {
        Self { mm_user }
    }

    /// Locks the live address space.
    pub fn lock(&self) -> MutexGuard<'_, MmSpace> {
        self.mm_user.address_space().lock()
    }

    /// Runs a mapping operation while holding the live mm user capability.
    pub fn with_mapping_owner<R>(
        &self,
        f: impl FnOnce(LiveAddressSpaceMappingGuard<'_>) -> R,
    ) -> R {
        let owner = self.mm_user.address_space();
        let guard = owner.lock();
        f(LiveAddressSpaceMappingGuard { owner, guard })
    }
}

/// Mapping guard available only while a live address-space capability is held.
pub struct LiveAddressSpaceMappingGuard<'a> {
    owner: &'a Arc<Mutex<MmSpace>>,
    guard: MutexGuard<'a, MmSpace>,
}

impl LiveAddressSpaceMappingGuard<'_> {
    /// Returns the guarded address space.
    pub fn aspace(&self) -> &MmSpace {
        &self.guard
    }

    /// Returns the guarded address space for mutation.
    pub fn aspace_mut(&mut self) -> &mut MmSpace {
        &mut self.guard
    }

    /// Creates an observer for mapping runtime callbacks.
    pub fn observer(&self) -> MmObserver {
        MmObserver::new(self.owner)
    }

    /// Creates an invalidation handle for mapping runtime callbacks.
    pub fn invalidate_handle(&self) -> InvalidateHandle {
        self.guard.invalidate_handle(self.owner)
    }

    /// Installs a relocated mapping snapshot using this live owner.
    pub fn map_relocated_snapshot(
        &mut self,
        snapshot: &MremapSource,
        new_start: VirtAddr,
        new_size: usize,
        new_flags: MappingFlags,
    ) -> KResult {
        self.guard
            .map_relocated_snapshot(snapshot, new_start, new_size, new_flags, self.owner)
    }
}

/// Process-shared state updates that become visible after a successful exec.
pub struct ProcessExecUpdate {
    pub(super) exe_path: String,
    pub(super) cmdline: Arc<Vec<String>>,
    pub(super) heap_top: usize,
    #[cfg(feature = "tee")]
    pub(super) ta_head_bytes: Vec<u8>,
}

impl ProcessExecUpdate {
    /// Creates a process exec update with the new executable metadata and heap top.
    pub fn new(exe_path: String, cmdline: Arc<Vec<String>>, heap_top: usize) -> Self {
        Self {
            exe_path,
            cmdline,
            heap_top,
            #[cfg(feature = "tee")]
            ta_head_bytes: Vec::new(),
        }
    }

    /// Installs the TEE TA header bytes that should be applied on exec.
    #[cfg(feature = "tee")]
    pub fn with_ta_head_bytes(mut self, ta_head_bytes: Vec<u8>) -> Self {
        self.ta_head_bytes = ta_head_bytes;
        self
    }
}

impl Process {
    /// Records a weak reference to the runtime owned by this process.
    pub(crate) fn set_runtime_ref(&self, runtime: &Arc<ProcessRuntime>) {
        *self.runtime_ref.lock() = Some(Arc::downgrade(runtime));
    }

    /// Returns the process runtime while a strong owner still exists.
    pub(crate) fn runtime_ref(&self) -> Option<Arc<ProcessRuntime>> {
        self.runtime_ref.lock().as_ref().and_then(Weak::upgrade)
    }

    fn runtime(&self) -> KResult<Arc<ProcessRuntime>> {
        self.runtime_ref().ok_or(KError::NoSuchProcess)
    }

    /// Returns the process-owned resource table while runtime remains attached.
    pub fn resources(&self) -> KResult<Arc<kresources::ProcessResources>> {
        self.runtime().map(|runtime| runtime.resources().clone())
    }

    /// Returns the filesystem context while runtime remains attached.
    pub fn fs_context(&self) -> KResult<Arc<Mutex<FsStruct>>> {
        self.runtime().map(|runtime| runtime.fs_context())
    }

    /// Returns the process UTS namespace while runtime remains attached.
    pub fn uts_ns(&self) -> KResult<Arc<kns::UtsNamespace>> {
        self.runtime().map(|runtime| runtime.uts_ns())
    }

    /// Returns the process mount namespace while runtime remains attached.
    pub fn mnt_ns(&self) -> KResult<Arc<kns::MntNamespace>> {
        self.runtime()
            .map(|runtime| runtime.nsproxy().mnt_ns().clone())
    }

    /// Returns a live address-space capability while runtime remains attached.
    pub fn address_space(&self) -> KResult<LiveAddressSpace> {
        self.runtime()
            .and_then(|runtime| runtime.address_space().ok_or(KError::NoSuchProcess))
    }

    /// Returns the pinned address-space object for teardown-state observation.
    #[cfg(unittest)]
    pub(crate) fn pinned_address_space_for_teardown_observation(
        &self,
    ) -> KResult<Arc<Mutex<MmSpace>>> {
        self.runtime().map(|runtime| {
            runtime
                .pinned_address_space_for_teardown_observation()
                .clone()
        })
    }

    /// Returns the immutable address-space identity while runtime remains attached.
    pub fn mm_id(&self) -> KResult<u64> {
        self.runtime().map(|runtime| runtime.mm_id())
    }

    /// Releases this process runtime's address-space user.
    ///
    /// Returns `true` when this was the last runtime user and user mappings
    /// were cleared.
    pub fn clear_exclusive_address_space(&self) -> KResult<bool> {
        self.runtime()
            .map(|runtime| runtime.clear_exclusive_address_space())
    }

    /// Returns the signal manager while runtime remains attached.
    pub fn signal_manager(&self) -> KResult<Arc<ProcessSignalManager>> {
        self.runtime()
            .map(|runtime| runtime.signal_manager().clone())
    }

    /// Returns the shared signal actions while runtime remains attached.
    pub fn signal_actions(&self) -> KResult<Arc<KSyncSpinNoIrq<SignalActions>>> {
        self.signal_manager().map(|signal| signal.actions.clone())
    }

    /// Returns a stable objective credential snapshot for one published thread.
    pub fn credentials_snapshot(&self) -> KResult<Arc<Cred>> {
        self.thread_tasks()
            .into_iter()
            .next()
            .map(|task| task.as_thread().real_cred())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns the current executable path while runtime remains attached.
    pub fn exe_path(&self) -> KResult<String> {
        self.runtime().map(|runtime| runtime.exe_path())
    }

    /// Returns the current command line while runtime remains attached.
    pub fn cmdline(&self) -> KResult<Arc<Vec<String>>> {
        self.runtime().map(|runtime| runtime.cmdline())
    }

    /// Returns the current process umask while runtime remains attached.
    pub fn umask(&self) -> KResult<u32> {
        let fs_context = self.fs_context()?;
        let umask = fs_context.lock().umask();
        Ok(umask)
    }

    /// Replaces the current process umask and returns the previous value.
    pub fn replace_umask(&self, umask: u32) -> KResult<u32> {
        let fs_context = self.fs_context()?;
        let old_umask = fs_context.lock().replace_umask(umask);
        Ok(old_umask)
    }

    /// Returns the current process heap top while runtime remains attached.
    pub fn heap_top(&self) -> KResult<usize> {
        self.runtime().map(|runtime| runtime.heap_top())
    }

    /// Sets the current process heap top.
    pub fn set_heap_top(&self, top: usize) -> KResult<()> {
        self.runtime().map(|runtime| runtime.set_heap_top(top))
    }

    /// Creates a POSIX timer while runtime remains attached.
    pub fn create_posix_timer(
        &self,
        clock_id: i32,
        notify: PosixTimerCreateNotify,
    ) -> KResult<i32> {
        self.runtime().and_then(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .create_posix_timer(clock_id, notify)
        })
    }

    /// Returns the current POSIX timer state while runtime remains attached.
    pub fn get_posix_timer(&self, timer_id: i32) -> KResult<(TimeSpan, TimeSpan)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.runtime().and_then(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .get_posix_timer(timer_id, process_utime, process_stime)
        })
    }

    /// Sets a POSIX timer and returns its previous state and immediate delivery.
    pub fn set_posix_timer(
        &self,
        timer_id: i32,
        absolute: bool,
        interval: TimeSpan,
        value: TimeSpan,
    ) -> KResult<((TimeSpan, TimeSpan), Option<TimerDelivery>)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.runtime().and_then(|runtime| {
            runtime.timer_manager().lock().set_posix_timer(
                timer_id,
                absolute,
                interval,
                value,
                process_utime,
                process_stime,
            )
        })
    }

    /// Deletes a POSIX timer while runtime remains attached.
    pub fn delete_posix_timer(&self, timer_id: i32) -> KResult<()> {
        self.runtime()
            .and_then(|runtime| runtime.timer_manager().lock().delete_posix_timer(timer_id))
    }

    /// Returns the POSIX timer overrun count while runtime remains attached.
    pub fn get_posix_timer_overrun(&self, timer_id: i32) -> KResult<i32> {
        self.runtime().and_then(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .get_posix_timer_overrun(timer_id)
        })
    }

    /// Returns the current interval timer state while runtime remains attached.
    pub fn get_itimer(&self, timer_type: ITimerType) -> KResult<(TimeSpan, TimeSpan)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.runtime().map(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .get_itimer(timer_type, process_utime, process_stime)
        })
    }

    /// Sets an interval timer and returns its previous state.
    pub fn set_itimer(
        &self,
        timer_type: ITimerType,
        interval: TimeSpan,
        remaining: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.runtime().map(|runtime| {
            runtime.timer_manager().lock().set_itimer(
                timer_type,
                interval,
                remaining,
                process_utime,
                process_stime,
            )
        })
    }

    /// Polls wall-clock-driven timers while runtime remains attached.
    pub(crate) fn poll_wall_clock_timers(&self) -> KResult<Vec<TimerDelivery>> {
        self.runtime()
            .map(|runtime| runtime.timer_manager().lock().poll_wall_clock())
    }

    /// Polls CPU-driven timers while runtime remains attached.
    pub(crate) fn poll_cpu_timers(&self) -> KResult<Vec<TimerDelivery>> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.runtime().map(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .poll_cpu_timers(process_utime, process_stime)
        })
    }

    /// Updates POSIX timer signal accounting when a timer signal is dequeued.
    pub(crate) fn on_timer_signal_dequeued(&self, timer_id: i32, signal_seq: u32) -> KResult<bool> {
        self.runtime().map(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .on_timer_signal_dequeued(timer_id, signal_seq)
        })
    }

    /// Updates the current executable metadata snapshot.
    #[cfg(unittest)]
    pub(crate) fn set_exec_metadata(
        &self,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
    ) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.set_exec_metadata(exe_path, cmdline))
    }

    /// Applies the process-shared post-exec state transition.
    pub fn apply_exec_update(&self, update: ProcessExecUpdate) -> KResult<()> {
        let runtime = self.runtime()?;

        #[cfg(feature = "tee")]
        runtime.with_tee_ta_ctx_mut(|tee_ta_ctx| {
            tee_ta_ctx.init_ta_ctx(update.exe_path.as_str(), update.ta_head_bytes.as_slice());
        });
        runtime.set_heap_top(update.heap_top);
        runtime.reset_signal_actions();
        runtime.clear_posix_timers();
        runtime.resources().close_cloexec_files();
        #[cfg(feature = "tipc")]
        runtime.with_tipc_handles(|handles| handles.write().uctx_handle_close_all());
        #[cfg(feature = "tee")]
        runtime.clear_tee_runtime_private();
        runtime.set_exec_metadata(update.exe_path, update.cmdline);
        Ok(())
    }

    /// Resets process-wide signal actions to defaults.
    pub fn reset_signal_actions(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.reset_signal_actions())
    }

    /// Clears all process POSIX timers.
    pub fn clear_posix_timers(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.clear_posix_timers())
    }

    /// Closes all process file descriptors.
    pub fn close_all_fds(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.resources().close_all_fds())
    }

    /// Closes all process file descriptors marked `FD_CLOEXEC`.
    pub fn close_cloexec_files(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.resources().close_cloexec_files())
    }

    /// Closes every TIPC handle before this process starts its new executable.
    ///
    /// TIPC handles are process-local capabilities, separate from the POSIX
    /// file descriptor table, and therefore are not covered by `FD_CLOEXEC`.
    #[cfg(feature = "tipc")]
    pub fn close_all_tipc_handles(&self) -> KResult<()> {
        self.runtime().map(|runtime| {
            runtime.with_tipc_handles(|handles| handles.write().uctx_handle_close_all())
        })
    }

    /// Clears process-local TEE runtime private state.
    #[cfg(feature = "tee")]
    pub fn clear_tee_runtime_private(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.clear_tee_runtime_private())
    }

    /// Runs a closure with immutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx<R>(&self, f: impl FnOnce(&tee_task_iface::TeeTaCtx) -> R) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx(f))
    }

    /// Runs a closure with mutable access to the process-shared TEE TA context.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut tee_task_iface::TeeTaCtx) -> R,
    ) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx_mut(f))
    }

    /// Runs a closure with access to the process-local Trusty IPC handle table.
    #[cfg(feature = "tipc")]
    pub fn with_tipc_handles<R>(
        &self,
        f: impl FnOnce(&ksync::RwLock<tipc_handle::HandleTable>) -> R,
    ) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tipc_handles(f))
    }

    /// Returns sampled user and system CPU time.
    pub fn process_cpu_times(&self) -> (TimeSpan, TimeSpan) {
        let (utime, stime) = self.exited_thread_time();
        self.thread_tasks()
            .into_iter()
            .fold((utime, stime), |(utime, stime), task| {
                let (thread_utime, thread_stime) = task.as_thread().sample_cpu_time();
                (
                    utime.saturating_add(thread_utime),
                    stime.saturating_add(thread_stime),
                )
            })
    }

    /// Returns the total sampled CPU time.
    pub fn process_cpu_time(&self) -> TimeSpan {
        let (utime, stime) = self.process_cpu_times();
        utime.saturating_add(stime)
    }
}
