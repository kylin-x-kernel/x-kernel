// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Handle contracts, event bits, and one-shot waiter notification storage.

use core::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
};

use bitflags::bitflags;
use kpoll::{PollContext, PollRegisterError, PollSet};

bitflags! {
    /// Events reported by TIPC handles.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct HandleEventMask: u32 {
        /// No events are pending.
        const NONE = 0x00;
        /// A port has a pending connection or an asynchronous connect completed.
        const READY = 0x01;
        /// The handle entered an error state.
        const ERROR = 0x02;
        /// The channel peer disconnected.
        const HUP = 0x04;
        /// A complete message is available.
        const MSG = 0x08;
        /// A previously blocked sender may retry.
        const SEND_UNBLOCKED = 0x10;
    }
}

/// No events are pending.
pub const IPC_HANDLE_POLL_NONE: u32 = HandleEventMask::NONE.bits();
/// A port is ready or an asynchronous connection completed.
pub const IPC_HANDLE_POLL_READY: u32 = HandleEventMask::READY.bits();
/// A handle entered an error state.
pub const IPC_HANDLE_POLL_ERROR: u32 = HandleEventMask::ERROR.bits();
/// A channel peer disconnected.
pub const IPC_HANDLE_POLL_HUP: u32 = HandleEventMask::HUP.bits();
/// A complete message is available.
pub const IPC_HANDLE_POLL_MSG: u32 = HandleEventMask::MSG.bits();
/// A previously blocked sender may retry.
pub const IPC_HANDLE_POLL_SEND_UNBLOCKED: u32 = HandleEventMask::SEND_UNBLOCKED.bits();

/// Runtime type of a TIPC handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleKind {
    /// A published or unpublished service port.
    Port,
    /// One endpoint of a bidirectional connection.
    Channel,
    /// A set used to wait on multiple TIPC handles.
    HandleSet,
    /// A memory reference handle that can be transferred in messages.
    MemRef,
}

/// Common behavior shared by all TIPC kernel objects.
///
/// Implementations synchronize their state. [`Self::kind`] and [`Self::poll`]
/// run under a set's IRQ-disabling lock and must not block or re-enter it.
/// Registration and close may invoke wakers; callers must allow for re-entry.
pub trait Handle: Any + Send + Sync {
    /// Returns the concrete handle kind.
    fn kind(&self) -> HandleKind;

    /// Returns events currently visible on the object.
    ///
    /// `finalize = false` preserves events; `true` may consume edge-like events
    /// before the caller filters the returned mask.
    fn poll(&self, finalize: bool) -> HandleEventMask;

    /// Registers the wait represented by `context` for an event transition.
    ///
    /// `event_mask` selects interests but may allow extra wake-ups. Retain
    /// [`kpoll::PollRegistrations`] across pending polls and recheck readiness
    /// after registering or waking; see the [wait example](crate::HandleTable::register_wait_any_table_change).
    ///
    /// # Errors
    ///
    /// [`PollRegisterError::NoMemory`] on allocation failure,
    /// [`PollRegisterError::IdExhausted`] on exhausted registration IDs, or
    /// [`PollRegisterError::InvalidState`] if the target rejects registration.
    /// Partial registrations remain until the owner is cleared or dropped.
    fn register(
        &self,
        context: &mut PollContext<'_>,
        event_mask: HandleEventMask,
    ) -> Result<(), PollRegisterError>;

    /// Closes the object. Calling this more than once is harmless.
    fn close(&self);

    /// Stores an opaque caller cookie on this handle.
    fn set_cookie(&self, cookie: usize);

    /// Returns the opaque caller cookie.
    fn cookie(&self) -> usize;

    /// Returns whether this handle may be attached to a TIPC message.
    ///
    /// Defaults to `true`; this query neither transfers nor authorizes a send.
    fn is_sendable(&self) -> bool {
        true
    }

    /// Returns the concrete object for downcasting, including table cleanup.
    fn as_any(&self) -> &dyn Any;
}

/// Shared event notification and cookie storage embedded by TIPC objects.
///
/// Publish readiness before notifying. Notifications are one-shot hints;
/// the concrete object owns readiness, independently of the atomic cookie.
pub struct HandleWaitState {
    poll_set: PollSet,
    cookie: AtomicUsize,
}

impl HandleWaitState {
    /// Creates an allocated wait source with no registrations and a zero cookie.
    pub fn new() -> Self {
        Self {
            poll_set: PollSet::new(),
            cookie: AtomicUsize::new(0),
        }
    }

    /// Registers the wait represented by `context` for the next notification.
    ///
    /// Retain [`kpoll::PollRegistrations`] while pending; recheck readiness
    /// after registering and register again after waking. Clearing or dropping
    /// the owner cancels queued registrations.
    ///
    /// # Examples
    ///
    /// One poll round using the executor's context. Keep `owner` alive while
    /// the result is `false`; clear it on completion or error.
    ///
    /// ```no_run
    /// use core::task::Context;
    ///
    /// use kpoll::{PollRegisterError, PollRegistrations};
    /// use tipc_handle::HandleWaitState;
    ///
    /// fn poll_ready(
    ///     state: &HandleWaitState,
    ///     is_ready: impl Fn() -> bool,
    ///     owner: &mut PollRegistrations,
    ///     cx: &Context<'_>,
    /// ) -> Result<bool, PollRegisterError> {
    ///     state.register(&mut owner.context(cx))?;
    ///     Ok(is_ready()) // Recheck after registering to avoid missed wake-ups.
    /// }
    /// ```
    ///
    /// # Errors
    ///
    /// [`PollRegisterError::NoMemory`] if registration storage cannot grow, or
    /// [`PollRegisterError::IdExhausted`] if source registration IDs run out.
    pub fn register(&self, context: &mut PollContext<'_>) -> Result<(), PollRegisterError> {
        context.register(&self.poll_set)
    }

    /// Detaches and wakes all currently registered waiters.
    ///
    /// IRQ-safe; invokes wakers outside the source lock. Notifications are not
    /// retained, and cancellation may race with a final wake.
    pub fn notify(&self) {
        self.poll_set.wake();
    }

    /// Stores an opaque caller cookie with release ordering.
    pub fn set_cookie(&self, cookie: usize) {
        self.cookie.store(cookie, Ordering::Release);
    }

    /// Loads the opaque caller cookie with acquire ordering; initially zero.
    pub fn cookie(&self) -> usize {
        self.cookie.load(Ordering::Acquire)
    }
}

impl Default for HandleWaitState {
    fn default() -> Self {
        Self::new()
    }
}
