// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicUsize, Ordering};

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
/// Dropping it may lock the MM and tear down mappings. Release it only in
/// sleepable task context, outside spinlocks and the MM mutex.
/// Its heap marker belongs to the captured program image: later changes to
/// the process binding do not retarget this capability.
pub struct LiveAddressSpace {
    mm_user: MmUserHandle,
    heap_top: Arc<AtomicUsize>,
}

impl LiveAddressSpace {
    pub(crate) fn new(mm_user: MmUserHandle, heap_top: Arc<AtomicUsize>) -> Self {
        Self { mm_user, heap_top }
    }

    /// Returns the live heap top of the captured image, not the current process image.
    ///
    /// Does not acquire a lock; may be read while holding this capability's MM mutex.
    pub fn heap_top(&self) -> usize {
        self.heap_top.load(Ordering::Acquire)
    }

    /// Locks the live address space.
    pub fn lock(&self) -> MutexGuard<'_, MmSpace> {
        self.mm_user.address_space().lock()
    }

    /// Runs a mapping operation while holding the live mm user capability.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn inspect(process: &kprocess::Process) -> kerrno::KResult<()> {
    /// let address_space = process.address_space()?;
    /// address_space.with_mapping_owner(|mapping| {
    ///     let _identity = mapping.aspace().mm_id();
    /// });
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The supplied process must still own its address space. The callback runs
    /// with the address-space mutex held and must not lock it recursively.
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
    ///
    /// # Errors
    ///
    /// Forwards the validation/allocation errors of
    /// `memspace::MmSpace::map_relocated_snapshot` for the supplied mapping snapshot,
    /// address, byte size and permissions.
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

/// Owned process-state update bundle applied during committed exec.
/// Updates may become visible before final success; application is not transactional.
pub struct ProcessExecUpdate {
    #[cfg(unittest)]
    failure_hits: Option<Arc<AtomicUsize>>,
    pub(super) exe_path: String,
    pub(super) cmdline: Arc<Vec<String>>,
    #[cfg(feature = "tee")]
    pub(super) ta_head_bytes: Vec<u8>,
}

impl ProcessExecUpdate {
    /// Creates a process exec update with the new executable metadata.
    pub fn new(exe_path: String, cmdline: Arc<Vec<String>>) -> Self {
        Self {
            #[cfg(unittest)]
            failure_hits: None,
            exe_path,
            cmdline,
            #[cfg(feature = "tee")]
            ta_head_bytes: Vec::new(),
        }
    }

    /// Injects a request-local error after real exec metadata publication.
    #[cfg(unittest)]
    pub fn with_failure_probe(mut self, hits: Arc<AtomicUsize>) -> Self {
        self.failure_hits = Some(hits);
        self
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
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn resources(&self) -> KResult<Arc<kresources::ProcessResources>> {
        self.runtime().map(|runtime| runtime.resources().clone())
    }

    /// Returns the attached filesystem context.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or filesystem owner
    /// has been released.
    pub fn fs_context(&self) -> KResult<Arc<Mutex<FsStruct>>> {
        self.runtime()?.fs_context().ok_or(KError::NoSuchProcess)
    }

    /// Returns the process UTS namespace while its namespace owner is attached.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn uts_ns(&self) -> KResult<Arc<kns::UtsNamespace>> {
        self.runtime()?.uts_ns().ok_or(KError::NoSuchProcess)
    }

    /// Returns the process mount namespace while its namespace owner is attached.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn mnt_ns(&self) -> KResult<Arc<kns::MntNamespace>> {
        self.runtime()?
            .nsproxy()
            .map(|nsproxy| nsproxy.mnt_ns().clone())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns the process cgroup namespace while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the runtime or the required namespace/mm
    /// owner has been detached.
    pub fn cgroup_ns(&self) -> KResult<Arc<kns::CgroupNamespace>> {
        self.runtime()?
            .nsproxy()
            .map(|nsproxy| nsproxy.cgroup_ns().clone())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns a live address-space capability while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the runtime or the required namespace/mm
    /// owner has been detached.
    pub fn address_space(&self) -> KResult<LiveAddressSpace> {
        self.runtime()
            .and_then(|runtime| runtime.address_space().ok_or(KError::NoSuchProcess))
    }

    /// Returns the pinned address-space object for teardown-state observation.
    /// This unittest-only observer does not retain active user mappings. Use a
    /// weak reference when testing reclamation so the observer cannot mask it.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` when the process runtime is no longer reachable.
    #[cfg(unittest)]
    pub fn pinned_address_space_for_teardown_observation(&self) -> KResult<Arc<Mutex<MmSpace>>> {
        self.runtime()
            .map(|runtime| runtime.pinned_address_space_for_teardown_observation())
    }

    /// Returns the bound address-space identity while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn mm_id(&self) -> KResult<u64> {
        self.runtime().map(|runtime| runtime.mm_id())
    }

    /// Releases this process runtime's address-space owner.
    ///
    /// Returns `true` when this was the last runtime user and user mappings
    /// were cleared.
    /// Requires sleepable task context, with no spinlock or this MM's mutex held.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the process runtime is no longer
    /// reachable.
    pub fn exit_mm(&self) -> KResult<bool> {
        self.runtime().map(|runtime| runtime.exit_mm())
    }

    /// Returns the signal manager while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn signal_manager(&self) -> KResult<Arc<ProcessSignalManager>> {
        self.runtime()
            .map(|runtime| runtime.signal_manager().clone())
    }

    /// Returns the shared signal actions while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn signal_actions(&self) -> KResult<Arc<KSyncSpinNoIrq<SignalActions>>> {
        self.signal_manager().map(|signal| signal.actions.clone())
    }

    /// Returns a stable objective credential snapshot for one published thread.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the process has no published thread
    /// from which to take an objective credential snapshot.
    pub fn credentials_snapshot(&self) -> KResult<Arc<Cred>> {
        self.thread_tasks()
            .into_iter()
            .next()
            .map(|task| task.as_thread().real_cred())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns the current executable path while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn exe_path(&self) -> KResult<String> {
        self.runtime().map(|runtime| runtime.exe_path())
    }

    /// Returns the current command line while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn cmdline(&self) -> KResult<Arc<Vec<String>>> {
        self.runtime().map(|runtime| runtime.cmdline())
    }

    /// Returns the current process umask while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the runtime or filesystem context
    /// owner has been detached.
    pub fn umask(&self) -> KResult<u32> {
        let fs_context = self.fs_context()?;
        let umask = fs_context.lock().umask();
        Ok(umask)
    }

    /// Replaces the current process umask and returns the previous value.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the runtime or filesystem context
    /// owner has been detached.
    pub fn replace_umask(&self, umask: u32) -> KResult<u32> {
        let fs_context = self.fs_context()?;
        let old_umask = fs_context.lock().replace_umask(umask);
        Ok(old_umask)
    }

    /// Returns the current process heap top while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn heap_top(&self) -> KResult<usize> {
        self.runtime().map(|runtime| runtime.heap_top())
    }

    /// Sets the current process heap top.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn set_heap_top(&self, top: usize) -> KResult<()> {
        self.runtime().map(|runtime| runtime.set_heap_top(top))
    }

    /// Runs `f` with the process timer manager locked, then re-derives the
    /// CPU-timer polling gate from the post-`f` manager state.
    ///
    /// Single refresh point for `cpu_timers_armed`: every wrapper that
    /// mutates timer state or advances timers in a poll goes through here.
    /// The refresh also runs when `f` fails, so a manager method that errors
    /// after partially mutating state cannot leave the gate stale. The only
    /// other refresh is `ProcessRuntime::clear_posix_timers`, which process
    /// teardown reaches directly without this wrapper. Read-only timer
    /// queries deliberately bypass this helper to avoid the gate rescan.
    ///
    /// The armed-poll path performs one extra runtime lookup on top of the
    /// gate check; that path only runs while CPU timers exist.
    fn with_timer_manager_mut<R>(
        &self,
        f: impl FnOnce(&mut ktimer::ProcessTimerManager) -> KResult<R>,
    ) -> KResult<R> {
        let runtime = self.runtime()?;
        let mut manager = runtime.timer_manager().lock();
        let result = f(&mut manager);
        runtime.set_cpu_timers_armed(manager.any_cpu_timer_armed());
        result
    }

    /// Creates a POSIX timer while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when runtime is gone and forwards the
    /// matching `ktimer::ProcessTimerManager::create_posix_timer` error contract.
    pub fn create_posix_timer(
        &self,
        clock_id: i32,
        notify: PosixTimerCreateNotify,
    ) -> KResult<i32> {
        self.with_timer_manager_mut(|manager| manager.create_posix_timer(clock_id, notify))
    }

    /// Returns the current POSIX timer state while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when runtime is gone and forwards the
    /// matching `ktimer::ProcessTimerManager::get_posix_timer` error contract.
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
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when runtime is gone and forwards the
    /// matching `ktimer::ProcessTimerManager::set_posix_timer` error contract.
    pub fn set_posix_timer(
        &self,
        timer_id: i32,
        absolute: bool,
        interval: TimeSpan,
        value: TimeSpan,
    ) -> KResult<((TimeSpan, TimeSpan), Option<TimerDelivery>)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.with_timer_manager_mut(|manager| {
            manager.set_posix_timer(
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
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when runtime is gone and forwards the
    /// matching `ktimer::ProcessTimerManager::delete_posix_timer` error contract.
    pub fn delete_posix_timer(&self, timer_id: i32) -> KResult<()> {
        self.with_timer_manager_mut(|manager| manager.delete_posix_timer(timer_id))
    }

    /// Returns the POSIX timer overrun count while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when runtime is gone and forwards the
    /// matching `ktimer::ProcessTimerManager::get_posix_timer_overrun` error contract.
    pub fn get_posix_timer_overrun(&self, timer_id: i32) -> KResult<i32> {
        self.runtime().and_then(|runtime| {
            runtime
                .timer_manager()
                .lock()
                .get_posix_timer_overrun(timer_id)
        })
    }

    /// Returns the current interval timer state while runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
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
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn set_itimer(
        &self,
        timer_type: ITimerType,
        interval: TimeSpan,
        remaining: TimeSpan,
    ) -> KResult<(TimeSpan, TimeSpan)> {
        let (process_utime, process_stime) = self.process_cpu_times();
        self.with_timer_manager_mut(|manager| {
            Ok(manager.set_itimer(
                timer_type,
                interval,
                remaining,
                process_utime,
                process_stime,
            ))
        })
    }

    /// Polls wall-clock-driven timers while runtime remains attached.
    pub(crate) fn poll_wall_clock_timers(&self) -> KResult<Vec<TimerDelivery>> {
        self.runtime()
            .map(|runtime| runtime.timer_manager().lock().poll_wall_clock())
    }

    /// Polls CPU-driven timers while runtime remains attached.
    ///
    /// Gated by [`ProcessRuntime::cpu_timers_armed`]: processes without any
    /// CPU-time timer skip the process-wide CPU-time sampling, which is the
    /// dominant fixed cost of this poll. The gate is re-derived from the
    /// timer manager after every real poll so one-shot expiry and interval
    /// exhaustion clear it without an explicit disarm call.
    pub(crate) fn poll_cpu_timers(&self) -> KResult<Vec<TimerDelivery>> {
        let runtime = self.runtime()?;
        if !runtime.cpu_timers_armed() {
            return Ok(Vec::new());
        }
        let (process_utime, process_stime) = self.process_cpu_times();
        self.with_timer_manager_mut(|manager| {
            Ok(manager.poll_cpu_timers(process_utime, process_stime))
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
    /// The new MM binding already owns its initialized heap marker; this update
    /// does not reset or replace it.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` for an unreachable runtime and forwards
    /// `kresources::ProcessResources::close_cloexec_files` errors. Earlier
    /// signal and timer updates are not rolled back on that error; executable
    /// metadata is published only after cleanup succeeds.
    /// In unittest builds, a selected request may return `NoMemory` after
    /// publication; this exercises the caller's fatal policy, not rollback.
    pub fn apply_exec_update(&self, update: ProcessExecUpdate) -> KResult<()> {
        let runtime = self.runtime()?;

        #[cfg(feature = "tee")]
        runtime.with_tee_ta_ctx_mut(|tee_ta_ctx| {
            tee_ta_ctx.init_ta_ctx(update.exe_path.as_str(), update.ta_head_bytes.as_slice());
        });
        runtime.reset_signal_actions();
        runtime.clear_posix_timers();
        runtime.resources().close_cloexec_files()?;
        #[cfg(feature = "tipc")]
        runtime.with_tipc_handles(|handles| handles.write().uctx_handle_close_all());
        #[cfg(feature = "tee")]
        runtime.clear_tee_runtime_private();
        runtime.set_exec_metadata(update.exe_path, update.cmdline);
        #[cfg(unittest)]
        if let Some(hits) = update.failure_hits {
            hits.fetch_add(1, Ordering::Relaxed);
            return Err(KError::NoMemory);
        }
        Ok(())
    }

    /// Resets process-wide signal actions to defaults.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn reset_signal_actions(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.reset_signal_actions())
    }

    /// Clears all process POSIX timers.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    pub fn clear_posix_timers(&self) -> KResult<()> {
        self.runtime().map(|runtime| runtime.clear_posix_timers())
    }

    /// Releases this process's file descriptor table owner.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the process runtime is no longer
    /// reachable.
    pub fn exit_files(&self) -> KResult<()> {
        self.runtime()?.resources().exit_files();
        Ok(())
    }

    /// Releases this process's filesystem context owner.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the process runtime is no longer
    /// reachable.
    pub fn exit_fs(&self) -> KResult<()> {
        self.runtime()?.exit_fs();
        Ok(())
    }

    /// Releases this process's namespace owner.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the process runtime is no longer
    /// reachable.
    pub fn exit_namespaces(&self) -> KResult<()> {
        self.runtime()?.exit_namespaces();
        Ok(())
    }

    /// Closes all process file descriptors marked `FD_CLOEXEC`.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` for an unreachable runtime and forwards
    /// `kresources::ProcessResources::close_cloexec_files` errors.
    pub fn close_cloexec_files(&self) -> KResult<()> {
        self.runtime()?.resources().close_cloexec_files()
    }

    /// Closes every TIPC handle owned by this process.
    ///
    /// TIPC handles are process-local capabilities, separate from the POSIX
    /// file descriptor table, and therefore are not covered by `FD_CLOEXEC`.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    #[cfg(feature = "tipc")]
    pub fn close_all_tipc_handles(&self) -> KResult<()> {
        self.runtime().map(|runtime| {
            runtime.with_tipc_handles(|handles| handles.write().uctx_handle_close_all())
        })
    }

    /// Clears process-local TEE runtime private state.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    #[cfg(feature = "tee")]
    pub fn clear_tee_runtime_private(&self) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.clear_tee_runtime_private())
    }

    /// Runs a closure with immutable access to the process-shared TEE TA context.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx<R>(&self, f: impl FnOnce(&tee_task_iface::TeeTaCtx) -> R) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx(f))
    }

    /// Runs a closure with mutable access to the process-shared TEE TA context.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
    #[cfg(feature = "tee")]
    pub fn with_tee_ta_ctx_mut<R>(
        &self,
        f: impl FnOnce(&mut tee_task_iface::TeeTaCtx) -> R,
    ) -> KResult<R> {
        self.runtime().map(|runtime| runtime.with_tee_ta_ctx_mut(f))
    }

    /// Runs a closure with access to the process-local Trusty IPC handle table.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` when the weak process runtime can no longer
    /// be upgraded.
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
