// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ktask::KtaskRef;
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsInode};

use crate::{AsThread, Pid, Process, Tid, lookup};

/// Process capability file descriptor for monitoring process lifecycle changes.
pub struct PidFd {
    process: Arc<Process>,
    exit_event: Arc<PollSet>,
    /// The thread this pidfd refers to when it was created by a `clone3` with
    /// `CLONE_THREAD | CLONE_PIDFD`. `None` means the pidfd names the
    /// thread-group leader. Member-thread identities are allocated root-only,
    /// so this handle carries the thread's root tid rather than per-namespace
    /// projections.
    task_number: Option<Arc<kidentity::PidHandle>>,
    /// Stable identity of the task named by `task_number`, used to reject a
    /// reused tid or an exec leader takeover when resolving the fdinfo line.
    task_token: Option<kidentity::TaskIdentityToken>,
    /// PID namespace the thread lives in, captured when the pidfd is created.
    /// Threads share their process's namespace, and the root-only thread
    /// identity can only be reported to observers that would name that
    /// namespace (itself or an ancestor).
    task_pid_ns: Option<Arc<kidentity::PidNamespace>>,
}

impl PidFd {
    /// Create a new pidfd for the given process.
    pub fn new(process: &Arc<Process>) -> Self {
        Self {
            process: process.clone(),
            exit_event: process.exit_event().clone(),
            task_number: None,
            task_token: None,
            task_pid_ns: None,
        }
    }

    /// Create a new pidfd that reports a specific thread (its tid) in fdinfo,
    /// while still anchoring lifecycle and file lookup on the owning process.
    ///
    /// `task_number` carries every PID-namespace projection of the tid and
    /// `task_token` its stable identity, so [`Self::fdinfo_pid`] can resolve the
    /// number in the reader's namespace and reject a reused tid.
    fn new_with_task_pid(
        process: &Arc<Process>,
        task_number: Arc<kidentity::PidHandle>,
        task_token: kidentity::TaskIdentityToken,
    ) -> Self {
        Self {
            process: process.clone(),
            exit_event: process.exit_event().clone(),
            task_number: Some(task_number),
            task_token: Some(task_token),
            task_pid_ns: Some(process.active_pid_namespace()),
        }
    }

    /// Create the anonymous-inode file used by `pidfd_open`.
    ///
    /// `cred` is captured as the new file's immutable open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits; forwards
    /// anonymous-inode file construction errors.
    pub fn new_file(
        process: &Arc<Process>,
        open_flags: u32,
        cred: Arc<Cred>,
    ) -> KResult<Arc<VfsFile>> {
        Self::file_from_pidfd(Self::new(process), open_flags, cred)
    }

    /// Create the pidfd file for a clone that reports a specific task id (a
    /// thread's tid for `CLONE_THREAD | CLONE_PIDFD`) in fdinfo.
    ///
    /// # Errors
    ///
    /// Same as [`Self::new_file`].
    pub fn new_file_for_task(
        process: &Arc<Process>,
        task_number: Arc<kidentity::PidHandle>,
        task_token: kidentity::TaskIdentityToken,
        open_flags: u32,
        cred: Arc<Cred>,
    ) -> KResult<Arc<VfsFile>> {
        Self::file_from_pidfd(
            Self::new_with_task_pid(process, task_number, task_token),
            open_flags,
            cred,
        )
    }

    fn file_from_pidfd(pidfd: Self, open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[pidfd]",
            Arc::new(PidfdFops),
            Arc::new(pidfd),
            FMode::READ | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the pidfd object attached to a pidfd file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if the file has no PidFd private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns the published process identity.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns the `Pid:` value Linux's `pidfd_show_fdinfo()` would print, or
    /// `-1` when the referenced pid no longer has a task attached.
    ///
    /// The value is the `pid_nr_ns` view for `observer` — the number the reader
    /// names the task with — and `0` when the pid has no projection there.
    /// Process identities carry per-namespace projections. Member-thread
    /// identities are allocated root-only, so the kernel's own name for a
    /// thread is its root tid (`gettid(2)`, the `clone3` return value); it is
    /// reported to a reader in the thread's namespace or an ancestor, and `0`
    /// to a reader below or beside that branch, matching `pid_nr_ns`. `None`
    /// means the caller has no PID namespace (kernel task) and gets the
    /// internal number.
    ///
    /// A process pidfd reports the thread-group leader until it is reaped; a
    /// waitable zombie still reports its pid. A thread pidfd
    /// (`CLONE_THREAD | CLONE_PIDFD`) resolves its tid dynamically: once the
    /// thread exits, an exec leader takeover detaches its identity, or the tid
    /// is reused by another task, it reports `-1`.
    pub fn fdinfo_pid(&self, observer: Option<&Arc<kidentity::PidNamespace>>) -> i64 {
        if let (Some(number), Some(token)) = (&self.task_number, self.task_token) {
            return match lookup::task(number.root_nr()) {
                Ok(task) if task.as_thread().identity_token() == token => match observer {
                    // Member-thread identities are allocated root-only, so the
                    // kernel's own name for the thread is its root tid. Report
                    // it only to a reader that names the thread's namespace or
                    // an ancestor of it; a reader below or beside that branch
                    // sees no projection, as Linux `pid_nr_ns` returns 0.
                    Some(ns) => {
                        let visible = self
                            .task_pid_ns
                            .as_ref()
                            .is_none_or(|thread_ns| ns.is_ancestor_or_self(thread_ns));
                        if visible {
                            i64::from(number.root_nr())
                        } else {
                            0
                        }
                    }
                    None => i64::from(number.root_nr()),
                },
                _ => -1,
            };
        }
        if !self.process.has_attached_task() {
            return -1;
        }
        match observer {
            Some(ns) => i64::from(self.process.number_in_namespace(ns).unwrap_or(0)),
            None => i64::from(self.process.pid()),
        }
    }

    /// Gets a descriptor from the referenced thread-group leader's files.
    ///
    /// The leader's own binding is used because threads may hold different
    /// file tables; there is deliberately no fallback to another live thread.
    ///
    /// # Errors
    ///
    /// Returns `NoSuchProcess` (ESRCH) when the leader has no resolvable task
    /// or has detached its files -- including after the leader exits but before
    /// a surviving thread takes over the group identity in `exec` -- and
    /// `BadFileDescriptor` for an absent descriptor.
    pub fn get_file(&self, fd: i32) -> KResult<Arc<VfsFile>> {
        let task = lookup::task(self.live_process()?.pid())?;
        let thread = task.as_thread();
        if !Arc::ptr_eq(thread.process(), &self.process) {
            return Err(KError::NoSuchProcess);
        }
        thread.files().get_file(fd)
    }

    /// Returns the referenced process only while it is still live.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` once the referenced process has exited.
    pub fn live_process(&self) -> KResult<&Arc<Process>> {
        if self.process.is_exited() {
            return Err(kerrno::KError::NoSuchProcess);
        }
        Ok(&self.process)
    }
}

/// Resolves the process referenced by `pidfd_open`.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when the PID is unpublished.
/// Published zombie identities remain valid; PID zero selects the current process.
pub fn open_target_process(pid: Pid) -> KResult<Arc<Process>> {
    lookup::published_process(pid)
}

/// Resolves the task referenced by robust futex list syscalls.
///
/// # Errors
///
/// Returns `kerrno::KError::NoSuchProcess` when a nonzero TID has no live
/// published task. TID zero selects the current task.
pub fn robust_list_task(tid: Tid) -> KResult<KtaskRef> {
    lookup::task(tid)
}

impl Pollable for PidFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.process.is_exited());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.exit_event)?;
        }
        Ok(())
    }
}

struct PidfdFops;

impl PidfdFops {
    fn pidfd(file: &VfsFile) -> KResult<Arc<PidFd>> {
        PidFd::from_file(file)
    }
}

impl FileOperations for PidfdFops {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Some(Self::pidfd(file).map_or(IoEvents::ERR, |pidfd| pidfd.poll()))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(pidfd) = Self::pidfd(file) {
            pidfd.register(context, events)?;
        }
        Ok(())
    }
}

unittest::path_mod_test!(pidfd_tests, "../ut-tests/pidfd.rs");
