use alloc::sync::{Arc, Weak};
use core::task::Context;

use kcore::task::AsThread;
use kerrno::{KResult, k_bail};
use kpoll::{IoEvents, PollSet, Pollable};
use kprocess::{ProcessGroup, Session};
use kspin::SpinNoIrq;
use ktask::current;

pub struct JobControl {
    foreground: SpinNoIrq<Weak<ProcessGroup>>,
    session: SpinNoIrq<Weak<Session>>,
    poll_fg: PollSet,
}

impl Default for JobControl {
    fn default() -> Self {
        Self::new()
    }
}

impl JobControl {
    pub fn new() -> Self {
        Self {
            foreground: SpinNoIrq::new(Weak::new()),
            session: SpinNoIrq::new(Weak::new()),
            poll_fg: PollSet::new(),
        }
    }

    /// Check if the current process is in the foreground process group
    pub fn current_in_foreground(&self) -> bool {
        self.foreground
            .lock()
            .upgrade()
            .is_none_or(|pg| Arc::ptr_eq(&current().as_thread().proc_data.proc.group(), &pg))
    }

    /// Get the current foreground process group
    pub fn foreground(&self) -> Option<Arc<ProcessGroup>> {
        self.foreground.lock().upgrade()
    }

    /// Set the foreground process group for this terminal
    pub fn set_foreground(&self, pg: &Arc<ProcessGroup>) -> KResult<()> {
        let mut guard = self.foreground.lock();
        let weak = Arc::downgrade(pg);
        if Weak::ptr_eq(&weak, &*guard) {
            return Ok(());
        }

        let Some(session) = self.session.lock().upgrade() else {
            k_bail!(
                OperationNotPermitted,
                "No session associated with job control"
            );
        };
        if !Arc::ptr_eq(&pg.session(), &session) {
            k_bail!(
                OperationNotPermitted,
                "Process group does not belong to the session"
            );
        }

        *guard = weak;
        drop(guard);
        self.poll_fg.wake();
        Ok(())
    }

    /// Associate this terminal with a session
    pub fn set_session(&self, session: &Arc<Session>) {
        let mut guard = self.session.lock();
        assert!(guard.upgrade().is_none());
        *guard = Arc::downgrade(session);
    }
}

impl Pollable for JobControl {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.current_in_foreground());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            self.poll_fg.register(context.waker());
        }
    }
}
