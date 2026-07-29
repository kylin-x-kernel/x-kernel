// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Weak;
use core::{
    fmt,
    task::{Context, Waker},
};

use smallvec::SmallVec;

use crate::{
    PollSet,
    source::{PollSetInner, RegistrationToken},
};

const INLINE_REGISTRATIONS: usize = 8;

/// An error raised while registering a poll waiter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollRegisterError {
    /// Memory required for a registration could not be allocated.
    NoMemory,
    /// The source exhausted its non-reusable registration identity space.
    IdExhausted,
    /// The wait target is not in a state that can accept registrations.
    InvalidState,
}

impl fmt::Display for PollRegisterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMemory => write!(f, "out of memory while registering poll waiter"),
            Self::IdExhausted => write!(f, "poll registration identity space exhausted"),
            Self::InvalidState => write!(f, "poll target is not ready for registration"),
        }
    }
}

impl core::error::Error for PollRegisterError {}

/// Ownership guard for one poll registration.
///
/// Dropping the guard unregisters the waiter if the source has not already
/// detached it for wake-up. The guard holds only a weak source reference and
/// therefore cannot keep a device or file alive.
pub struct PollRegistration {
    source: Weak<PollSetInner>,
    token: Option<RegistrationToken>,
}

impl PollRegistration {
    pub(crate) fn new(source: Weak<PollSetInner>, token: RegistrationToken) -> Self {
        Self {
            source,
            token: Some(token),
        }
    }

    fn unregister(&mut self) -> bool {
        let Some(token) = self.token.take() else {
            return false;
        };
        self.source
            .upgrade()
            .is_some_and(|source| source.unregister(token))
    }

    /// Cancels this registration.
    ///
    /// Returns `true` when cancellation removed a queued waiter. A `false`
    /// result means the source was dropped or had already detached the waiter
    /// for wake-up, in which case one late wake may still occur.
    pub fn cancel(mut self) -> bool {
        self.unregister()
    }
}

impl Drop for PollRegistration {
    fn drop(&mut self) {
        self.unregister();
    }
}

type RegistrationList = SmallVec<[PollRegistration; INLINE_REGISTRATIONS]>;

/// Owns every source registration made by one logical wait operation.
///
/// This value must live across `Poll::Pending`. Dropping or clearing it
/// unregisters all still-queued waiters.
pub struct PollRegistrations {
    registrations: RegistrationList,
}

impl PollRegistrations {
    /// Creates an empty registration owner without allocating.
    pub const fn new() -> Self {
        Self {
            registrations: SmallVec::new_const(),
        }
    }

    /// Starts a new registration round for `context`.
    ///
    /// Registrations left from the preceding round are cancelled before the
    /// returned context can register new sources.
    pub fn context<'a>(&'a mut self, context: &'a Context<'_>) -> PollContext<'a> {
        self.clear();
        PollContext {
            waker: context.waker(),
            registrations: &mut self.registrations,
        }
    }

    /// Cancels every registration still owned by this wait operation.
    pub fn clear(&mut self) {
        self.registrations.clear();
    }

    /// Returns the number of registrations retained across `Poll::Pending`.
    pub fn len(&self) -> usize {
        self.registrations.len()
    }

    /// Returns whether no registrations are retained.
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
}

impl Default for PollRegistrations {
    fn default() -> Self {
        Self::new()
    }
}

/// Poll-scoped capability for registering the current task with sources.
///
/// A context borrows both the current [`Waker`] and its registration owner, so
/// Pollable implementations cannot retain an unmanaged waker.
pub struct PollContext<'a> {
    waker: &'a Waker,
    registrations: &'a mut RegistrationList,
}

impl Drop for PollContext<'_> {
    fn drop(&mut self) {
        // Ending the context ends the exclusive borrow of `PollRegistrations`,
        // allowing callers to recheck readiness while keeping registrations.
    }
}

impl PollContext<'_> {
    /// Registers the current logical wait with `source`.
    ///
    /// # Errors
    ///
    /// Returns [`PollRegisterError::NoMemory`] if either the registration
    /// owner or source cannot grow. Any source registration made before a
    /// later failure remains owned and is automatically rolled back when the
    /// owner is cleared or dropped.
    pub fn register(&mut self, source: &PollSet) -> Result<(), PollRegisterError> {
        self.registrations
            .try_reserve(1)
            .map_err(|_| PollRegisterError::NoMemory)?;
        let registration = source.register(self.waker)?;
        self.registrations.push(registration);
        Ok(())
    }

    /// Wakes the current task without registering on a source.
    ///
    /// Used by pollable objects that have no wait source and must force an
    /// immediate recheck, such as manual TTY input processing.
    pub fn wake_by_ref(&self) {
        self.waker.wake_by_ref();
    }
}
