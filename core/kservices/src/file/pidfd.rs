// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};
use core::task::Context;

use kerrno::{KError, KResult};
use kfd::FileLike;
use kpoll::{IoEvents, PollSet, Pollable};
use kthread::ProcessState;

/// Process file descriptor for monitoring process state changes.
///
/// A PidFd represents a reference to a process and can be used to monitor
/// when the process exits. It uses a weak reference to avoid preventing process cleanup.
pub struct PidFd {
    /// Weak reference to the process state to avoid keeping the process alive
    proc_state: Weak<ProcessState>,
    /// Event notification set for process exit events
    exit_event: Arc<PollSet>,
}
impl PidFd {
    /// Creates a new process file descriptor for the given process.
    pub fn new(proc_state: &Arc<ProcessState>) -> Self {
        Self {
            proc_state: Arc::downgrade(proc_state),
            exit_event: proc_state.exit_event().clone(),
        }
    }

    /// Retrieves the process state if the process is still alive.
    ///
    /// Returns `NoSuchProcess` if the process has already exited.
    pub fn process_state(&self) -> KResult<Arc<ProcessState>> {
        self.proc_state.upgrade().ok_or(KError::NoSuchProcess)
    }
}
impl FileLike for PidFd {
    /// Returns the path representation of this pidfd.
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[pidfd]".into()
    }
}

impl Pollable for PidFd {
    /// Polls for readable events (set to true when process is still alive).
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.proc_state.strong_count() > 0);
        events
    }

    /// Registers the pidfd for polling process exit events.
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

    #[def_test]
    fn test_ioevents_constants() {
        // Verify IoEvents has IN constant
        let events = IoEvents::IN;
        assert!(events.contains(IoEvents::IN));
    }
}
