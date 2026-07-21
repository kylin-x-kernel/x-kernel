// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Session management for process groups.
use alloc::sync::{Arc, Weak};
use core::{any::Any, fmt};

use kspin::SpinNoIrq;
use weak_map::WeakMap;

use crate::{Pid, ProcessGroup};

/// Controlling terminal object that can be bound to a POSIX session.
///
/// `kprocess` only owns the session slot and pointer-identity checks. Concrete
/// terminal behavior remains in the TTY subsystem.
pub trait ControllingTerminal: Any + Send + Sync {
    /// Converts this terminal object into an [`Any`] trait object for the
    /// owning subsystem that knows its concrete terminal types.
    fn into_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

/// Result of installing a controlling terminal in a [`Session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTerminalResult {
    /// The terminal was installed into an empty session slot.
    Installed,
    /// The session already points at the same terminal object.
    AlreadySetToSame,
    /// The session already has a different controlling terminal.
    Occupied,
}

/// A [`Session`] is a collection of [`ProcessGroup`]s.
pub struct Session {
    sid: Pid,
    pub(crate) process_groups: SpinNoIrq<WeakMap<Pid, Weak<ProcessGroup>>>,
    terminal: SpinNoIrq<Option<Arc<dyn ControllingTerminal>>>,
}

impl Session {
    /// Create a new [`Session`].
    pub(crate) fn new(sid: Pid) -> Arc<Self> {
        Arc::new(Self {
            sid,
            process_groups: SpinNoIrq::new(WeakMap::new()),
            terminal: SpinNoIrq::new(None),
        })
    }
}

impl Session {
    /// The [`Session`] ID.
    pub fn sid(&self) -> Pid {
        self.sid
    }

    /// Sets the terminal for this session.
    pub fn set_terminal(&self, terminal: &Arc<dyn ControllingTerminal>) -> SetTerminalResult {
        let mut guard = self.terminal.lock();
        match guard.as_ref() {
            Some(current) if Arc::ptr_eq(current, terminal) => SetTerminalResult::AlreadySetToSame,
            Some(_) => SetTerminalResult::Occupied,
            None => {
                *guard = Some(terminal.clone());
                SetTerminalResult::Installed
            }
        }
    }

    /// Unsets the terminal for this session if it is the given terminal.
    pub fn unset_terminal(&self, term: &Arc<dyn ControllingTerminal>) -> bool {
        let mut guard = self.terminal.lock();
        if guard.as_ref().is_some_and(|it| Arc::ptr_eq(it, term)) {
            *guard = None;
            true
        } else {
            false
        }
    }

    /// Gets the terminal for this session, if it exists.
    pub fn terminal(&self) -> Option<Arc<dyn ControllingTerminal>> {
        self.terminal.lock().clone()
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Session({})", self.sid)
    }
}
