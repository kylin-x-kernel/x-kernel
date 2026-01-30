use alloc::{
    borrow::Cow,
    sync::{Arc, Weak},
};
use core::task::Context;

use kcore::task::ProcessData;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollSet, Pollable};

use crate::file::FileLike;

/// Process file descriptor for monitoring process state changes.
///
/// A PidFd represents a reference to a process and can be used to monitor
/// when the process exits. It uses a weak reference to avoid preventing process cleanup.
pub struct PidFd {
    /// Weak reference to the process data to avoid keeping the process alive
    proc_data: Weak<ProcessData>,
    /// Event notification set for process exit events
    exit_event: Arc<PollSet>,
}
impl PidFd {
    /// Creates a new process file descriptor for the given process.
    pub fn new(proc_data: &Arc<ProcessData>) -> Self {
        Self {
            proc_data: Arc::downgrade(proc_data),
            exit_event: proc_data.exit_event.clone(),
        }
    }

    /// Retrieves the process data if the process is still alive.
    ///
    /// Returns `NoSuchProcess` if the process has already exited.
    pub fn process_data(&self) -> KResult<Arc<ProcessData>> {
        self.proc_data.upgrade().ok_or(KError::NoSuchProcess)
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
        events.set(IoEvents::IN, self.proc_data.strong_count() > 0);
        events
    }

    /// Registers the pidfd for polling process exit events.
    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.exit_event.register(context.waker());
        }
    }
}
