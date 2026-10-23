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
}

impl PidFd {
    /// Create a new pidfd for the given process.
    pub fn new(process: &Arc<Process>) -> Self {
        Self {
            process: process.clone(),
            exit_event: process.exit_event().clone(),
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
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[pidfd]",
            Arc::new(PidfdFops),
            Arc::new(Self::new(process)),
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

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::pidfd(file).map_or(IoEvents::ERR, |pidfd| pidfd.poll())
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
