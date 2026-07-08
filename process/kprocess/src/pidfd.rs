// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{borrow::Cow, sync::Arc};
use core::task::Context;

use kerrno::KResult;
use kfd::FileLike;
use kpoll::{IoEvents, PollSet, Pollable};
use ktask::KtaskRef;

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

    /// Returns the published process identity.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Returns the referenced process only while it is still live.
    pub fn live_process(&self) -> KResult<&Arc<Process>> {
        if self.process.is_zombie() {
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

impl FileLike for PidFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[pidfd]".into()
    }
}

impl Pollable for PidFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.process.is_zombie());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.exit_event.register(context.waker());
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

        wait_reap::reap_zombie_process(&proc);
    }
}
