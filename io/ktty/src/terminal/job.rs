// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::{Arc, Weak};

use kerrno::{KError, KResult, k_bail};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use kprocess::{ProcessGroup, Session};
use kspin::SpinNoIrq;

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
            .is_none_or(|pg| Arc::ptr_eq(&kprocess::current_user_thread().process().group(), &pg))
    }

    /// Get the current foreground process group
    pub fn foreground(&self) -> Option<Arc<ProcessGroup>> {
        self.foreground.lock().upgrade()
    }

    /// Returns the foreground group when `caller_session` owns this terminal.
    pub fn foreground_for(&self, caller_session: &Arc<Session>) -> KResult<Arc<ProcessGroup>> {
        let foreground = self.foreground.lock();
        let terminal_session = self.session.lock().upgrade().ok_or(KError::NotATty)?;
        if !Arc::ptr_eq(caller_session, &terminal_session) {
            return Err(KError::NotATty);
        }
        foreground.upgrade().ok_or(KError::NotATty)
    }

    /// Get the session currently associated with this terminal
    pub fn session(&self) -> Option<Arc<Session>> {
        self.session.lock().upgrade()
    }

    /// Returns the associated session when it matches `caller_session`.
    pub fn session_for(&self, caller_session: &Arc<Session>) -> KResult<Arc<Session>> {
        let terminal_session = self.session.lock().upgrade().ok_or(KError::NotATty)?;
        if !Arc::ptr_eq(caller_session, &terminal_session) {
            return Err(KError::NotATty);
        }
        Ok(terminal_session)
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

    /// Sets the foreground process group on behalf of a caller in `caller_session`.
    ///
    /// # Errors
    ///
    /// Returns `ENOTTY` when the caller does not belong to the session that
    /// owns this terminal, and `EPERM` when `pg` belongs to another session.
    pub(crate) fn set_foreground_for(
        &self,
        caller_session: &Arc<Session>,
        pg: &Arc<ProcessGroup>,
    ) -> KResult<()> {
        let terminal_session = self.session.lock().upgrade().ok_or(KError::NotATty)?;
        if !Arc::ptr_eq(caller_session, &terminal_session) {
            return Err(KError::NotATty);
        }
        self.set_foreground(pg)
    }

    /// Ensures this terminal is associated with `session`.
    ///
    /// Returns `true` when a new association was installed.
    pub fn ensure_session(&self, session: &Arc<Session>) -> KResult<bool> {
        let mut guard = self.session.lock();
        if let Some(current) = guard.upgrade() {
            return if Arc::ptr_eq(&current, session) {
                Ok(false)
            } else {
                Err(KError::ResourceBusy)
            };
        }

        *guard = Arc::downgrade(session);
        Ok(true)
    }

    /// Clears this terminal's session association if it matches `session`.
    pub fn clear_session_if_matches(&self, session: &Arc<Session>) -> bool {
        let mut foreground = self.foreground.lock();
        let mut guard = self.session.lock();
        if guard
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, session))
        {
            *guard = Weak::new();
            *foreground = Weak::new();
            drop(guard);
            drop(foreground);
            self.poll_fg.wake();
            true
        } else {
            false
        }
    }
}

impl Pollable for JobControl {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.current_in_foreground());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_fg)?;
        }
        Ok(())
    }
}
