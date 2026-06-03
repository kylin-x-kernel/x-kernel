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

/// Process file descriptor for monitoring process lifecycle changes.
pub struct PidFd {
    proc_state: Weak<ProcessState>,
    exit_event: Arc<PollSet>,
}

impl PidFd {
    /// Create a new pidfd for the given process state.
    pub fn new(proc_state: &Arc<ProcessState>) -> Self {
        Self {
            proc_state: Arc::downgrade(proc_state),
            exit_event: proc_state.exit_event().clone(),
        }
    }

    /// Upgrade the weak process reference.
    pub fn process_state(&self) -> KResult<Arc<ProcessState>> {
        self.proc_state.upgrade().ok_or(KError::NoSuchProcess)
    }
}

impl FileLike for PidFd {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[pidfd]".into()
    }
}

impl Pollable for PidFd {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.proc_state.strong_count() > 0);
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

    #[def_test]
    fn test_ioevents_constants() {
        let events = IoEvents::IN;
        assert!(events.contains(IoEvents::IN));
    }
}
