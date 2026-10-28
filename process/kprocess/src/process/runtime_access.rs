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
use ksignal::api::ProcessSignalManager;
use ksync::{Mutex, MutexGuard};
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

    /// Relocates a mapping snapshot transactionally using this live owner.
    ///
    /// # Errors
    ///
    /// Forwards the validation/allocation errors of
    /// `memspace::MmSpace::relocate_mapping` for the supplied mapping snapshot,
    /// address, byte size and permissions.
    pub fn relocate_mapping(
        &mut self,
        snapshot: &MremapSource,
        new_start: VirtAddr,
        new_size: usize,
        new_flags: MappingFlags,
    ) -> KResult {
        self.guard
            .relocate_mapping(snapshot, new_start, new_size, new_flags, self.owner)
    }
}

/// Owned process-state update bundle applied during committed exec.
/// Updates may become visible before final success; application is not transactional.
pub struct ProcessExecUpdate {
    #[cfg(unittest)]
    failure_hits: Option<Arc<AtomicUsize>>,
    pub(super) exe_path: String,
    pub(super) cmdline: Arc<Vec<String>>,
    /// Opened backing executable file, exposed through `/proc/<pid>/exe`.
    pub(super) exe_file: Option<Arc<kvfs::VfsFile>>,
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
            exe_file: None,
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

    /// Installs the opened backing executable file for `/proc/<pid>/exe`.
    pub fn with_exe_file(mut self, exe_file: Arc<kvfs::VfsFile>) -> Self {
        self.exe_file = Some(exe_file);
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

    /// Returns the process namespace bundle while the runtime remains attached.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn nsproxy(&self) -> KResult<Arc<kns::NsProxy>> {
        self.runtime()?.nsproxy().ok_or(KError::NoSuchProcess)
    }

    /// Returns the process IPC namespace identity while its owner is attached.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn ipc_ns(&self) -> KResult<Arc<kns::IpcNamespace>> {
        self.runtime()?
            .nsproxy()
            .map(|nsproxy| nsproxy.ipc_ns().clone())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns the process network namespace identity while its owner is attached.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn net_ns(&self) -> KResult<Arc<kns::NetNamespace>> {
        self.runtime()?
            .nsproxy()
            .map(|nsproxy| nsproxy.net_ns().clone())
            .ok_or(KError::NoSuchProcess)
    }

    /// Returns the PID namespace used for future children of this process.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime or namespace owner
    /// has been released.
    pub fn pid_ns_for_children(&self) -> KResult<Arc<kns::PidNamespace>> {
        self.runtime()?
            .nsproxy()
            .map(|nsproxy| nsproxy.pid_ns_for_children().clone())
            .ok_or(KError::NoSuchProcess)
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

    /// Returns whether the process `fs_struct` is shared with at least one
    /// other task.
    ///
    /// Threads of the same process always share the filesystem context, and
    /// `CLONE_FS` forks share it across processes. X-Kernel cannot count the
    /// latter without a user counter, so the check covers published threads of
    /// this process as the conservative in-range subset of Linux's `fs->users`.
    pub fn fs_is_shared(&self) -> bool {
        self.threads().len() > 1
    }

    /// Applies a validated [`kns::PreparedUnshare`] bundle to this process.
    ///
    /// Unshare preparation has already performed authorization and combination
    /// checks; this only installs the precomputed namespace bundle and
    /// optionally replaces the filesystem context (`NEWNS`/`CLONE_FS`). The
    /// `CLONE_FILES` descriptor-table detach is thread-owned state: the syscall
    /// adapter commits it on the calling thread after this bundle install, so
    /// sibling threads keep their own bindings.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime is gone.
    pub fn apply_prepared_unshare(&self, prepared: kns::PreparedUnshare) -> KResult<()> {
        let runtime = self.runtime()?;
        runtime.set_nsproxy(prepared.nsproxy().clone());
        if let Some(new_fs) = prepared.new_fs() {
            runtime.set_fs_context(Arc::new(Mutex::new(new_fs.snapshot())));
        }
        Ok(())
    }

    /// Enters the namespaces of `new_proxy`, optionally retargeting the process
    /// filesystem root/pwd into a mount namespace.
    ///
    /// `target_mnt` retargets the fs context so `CLONE_NEWNS` setns also
    /// switches the VFS view. The caller has already authorized the switch and
    /// validated that the namespace kind is supported.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] when the runtime is gone, or the
    /// filesystem-context retarget error when the mount namespace's visible
    /// root is not a directory. Nothing is committed when an error is
    /// returned: the retarget is drafted on a snapshot before the bundle
    /// install.
    ///
    /// # Examples
    ///
    /// The `kns` prepare/validate phase runs first; this commit installs the
    /// already-validated result. For `setns(2)` on a mount-namespace fd, the
    /// commit also retargets root/pwd into the target namespace:
    ///
    /// ```no_run
    /// # fn enter_mount_ns(
    /// #     process: &kprocess::Process,
    /// #     nsfd: alloc::sync::Arc<kns::NsFd>,
    /// # ) -> kerrno::KResult<()> {
    /// // The NsFd channel already checked `nstype` and the caller's privilege.
    /// let target_mnt = match &*nsfd {
    ///     kns::NsFd::Mnt(ns) => Some(ns.clone()),
    ///     _ => None,
    /// };
    /// let new_proxy = process
    ///     .nsproxy()?
    ///     .with_namespace(&nsfd)
    ///     .expect("kind was validated before the commit");
    /// process.apply_setns(new_proxy, target_mnt)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn apply_setns(
        &self,
        new_proxy: Arc<kns::NsProxy>,
        target_mnt: Option<Arc<kns::MntNamespace>>,
    ) -> KResult<()> {
        let runtime = self.runtime()?;
        // The filesystem retarget is the only fallible step, so it is drafted
        // against a snapshot before anything is committed: a failed
        // `CLONE_NEWNS` switch must leave the namespace bundle and the fs
        // context exactly as they were.
        let new_fs = match target_mnt {
            Some(mnt_ns) => {
                let current_fs = runtime.fs_context().ok_or(KError::NoSuchProcess)?;
                let mut fs = current_fs.lock().snapshot();
                // kvfs::Path carries dentry/mount identity rather than a comparable
                // value, so the fs root and pwd are both retargeted to the target
                // namespace's visible root. This mirrors the same-namespace reentry
                // case that bind-mounted sandbox namespaces exercise.
                let root = mnt_ns.visible_root_path();
                fs.replace_root_and_pwd(root.clone(), root)?;
                Some(fs)
            }
            None => None,
        };
        runtime.set_nsproxy(new_proxy);
        if let Some(fs) = new_fs {
            runtime.set_fs_context(Arc::new(Mutex::new(fs)));
        }
        Ok(())
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

    /// Returns the opened backing executable file, if any, while runtime
    /// remains attached. `/proc/<pid>/exe` follows to this file.
    pub fn exe_file(&self) -> KResult<Option<Arc<kvfs::VfsFile>>> {
        self.runtime().map(|runtime| runtime.exe_file())
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
        self.set_cpu_timers_armed(manager.any_cpu_timer_armed());
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
    /// The user-return entry point [`crate::poll_cpu_timers_for`] gates the
    /// poll on [`Process::cpu_timers_armed`]: it reads the gate on the
    /// borrowed process without upgrading the weak runtime handle, so a
    /// timer-free process never reaches this method. This method re-reads the
    /// same gate purely to skip the process-wide CPU-time sample
    /// ([`Self::process_cpu_times`], the dominant fixed cost) when a
    /// concurrent disarm flips the gate between the two reads. The gate is
    /// re-derived from the timer manager after every real poll so one-shot
    /// expiry and interval exhaustion clear it without an explicit disarm call.
    pub(crate) fn poll_cpu_timers(&self) -> KResult<Vec<TimerDelivery>> {
        if !self.cpu_timers_armed() {
            return Ok(Vec::new());
        }
        let (process_utime, process_stime) = self.process_cpu_times();
        self.with_timer_manager_mut(|manager| {
            Ok(manager.poll_cpu_timers(process_utime, process_stime))
        })
    }

    /// Updates POSIX timer signal accounting when a timer signal is dequeued.
    pub(crate) fn on_timer_signal_dequeued(&self, timer_id: i32, signal_seq: u32) -> KResult<bool> {
        let runtime = self.runtime()?;
        // The sample walks the thread table, so only pay for it when this
        // dequeue will consume it: a stale generation or a timer whose clock
        // ignores the totals never does. The read-only check deliberately
        // bypasses `with_timer_manager_mut`; the mutation below goes through it.
        let needs_cpu_times = runtime
            .timer_manager()
            .lock()
            .posix_timer_dequeue_needs_cpu_times(timer_id, signal_seq);
        let (process_utime, process_stime) = if needs_cpu_times {
            self.process_cpu_times()
        } else {
            (TimeSpan::ZERO, TimeSpan::ZERO)
        };
        self.with_timer_manager_mut(|manager| {
            Ok(
                manager.on_timer_signal_dequeued(
                    timer_id,
                    signal_seq,
                    process_utime,
                    process_stime,
                ),
            )
        })
    }

    /// Updates the current executable metadata snapshot.
    #[cfg(unittest)]
    pub(crate) fn set_exec_metadata(
        &self,
        exe_path: String,
        cmdline: Arc<Vec<String>>,
        exe_file: Option<Arc<kvfs::VfsFile>>,
    ) -> KResult<()> {
        self.runtime()
            .map(|runtime| runtime.set_exec_metadata(exe_path, cmdline, exe_file))
    }

    /// Applies the process-shared post-exec state transition.
    /// The new MM binding already owns its initialized heap marker; this update
    /// does not reset or replace it.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` for an unreachable runtime.
    /// In unittest builds, a selected request may return `NoMemory` after
    /// publication; this exercises the caller's fatal policy, not rollback.
    pub(crate) fn apply_exec_update(&self, update: ProcessExecUpdate) -> KResult<()> {
        let runtime = self.runtime()?;

        #[cfg(feature = "tee")]
        runtime.with_tee_ta_ctx_mut(|tee_ta_ctx| {
            tee_ta_ctx.init_ta_ctx(update.exe_path.as_str(), update.ta_head_bytes.as_slice());
        });
        runtime.reset_signal_actions()?;
        runtime.clear_posix_timers();
        #[cfg(feature = "tipc")]
        runtime.with_tipc_handles(|handles| handles.write().uctx_handle_close_all());
        #[cfg(feature = "tee")]
        runtime.clear_tee_runtime_private();
        runtime.set_exec_metadata(update.exe_path, update.cmdline, update.exe_file);
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
        self.runtime()?.reset_signal_actions()
    }

    /// Releases signal configuration ownership during last-thread cleanup.
    /// Idempotent. Call outside process-domain, membership and signal locks,
    /// before publishing process completion to the parent.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` when the runtime is no longer reachable.
    pub fn exit_signal_handlers(&self) -> KResult<()> {
        self.runtime()?.signal_manager().release_handlers();
        Ok(())
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
