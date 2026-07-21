// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::task::Context;

use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollSet, Pollable};
use ktask::KtaskRef;
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsInode};

use crate::{Pid, Process, Tid, lookup};

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
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns the published process identity.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns the referenced process only while it is still live.
    pub fn live_process(&self) -> KResult<&Arc<Process>> {
        if self.process.is_exited() {
            return Err(kerrno::KError::NoSuchProcess);
        }
        Ok(&self.process)
    }
}

/// Resolves the process referenced by `pidfd_open`.
pub fn open_target_process(pid: Pid) -> KResult<Arc<Process>> {
    lookup::published_process(pid)
}

/// Resolves the task referenced by robust futex list syscalls.
pub fn robust_list_task(tid: Tid) -> KResult<KtaskRef> {
    lookup::task(tid)
}

impl Pollable for PidFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.process.is_exited());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.exit_event.register(context.waker());
        }
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

    fn register_poll(&self, file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        if let Ok(pidfd) = Self::pidfd(file) {
            pidfd.register(context, events);
        }
    }
}

#[cfg(unittest)]
mod pidfd_tests {
    use unittest::def_test;

    use super::*;
    use crate::{process_exit, wait_reap};

    #[def_test]
    fn test_ioevents_constants() {
        let events = IoEvents::IN;
        assert!(events.contains(IoEvents::IN));
    }

    #[def_test(serial)]
    fn test_pidfd_poll_becomes_readable_after_exit() {
        let proc = Process::new_init(9000).fork(9001);
        let pidfd = PidFd::new(&proc);

        assert!(
            !pidfd.poll().contains(IoEvents::IN),
            "live process must not make pidfd readable"
        );

        process_exit::finalize_process_exit(&proc);

        assert!(
            pidfd.poll().contains(IoEvents::IN),
            "exited zombie must make pidfd readable"
        );

        wait_reap::assert_reap_zombie_process(&proc);
    }
}
