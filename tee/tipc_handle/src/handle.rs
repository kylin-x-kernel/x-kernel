// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    any::Any,
    sync::atomic::{AtomicUsize, Ordering},
    task::Context,
};

use bitflags::bitflags;
use kpoll::PollSet;

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
pub trait Handle: Any + Send + Sync {
    /// Returns the concrete handle kind.
    fn kind(&self) -> HandleKind;

    /// Returns events currently visible on the object.
    ///
    /// If `finalize` is true, edge-like events such as `READY` after accept and
    /// `SEND_UNBLOCKED` are consumed.
    fn poll(&self, finalize: bool) -> HandleEventMask;

    /// Registers the current task for an event transition.
    fn register(&self, cx: &mut Context<'_>, event_mask: HandleEventMask);

    /// Closes the object. Calling this more than once is harmless.
    fn close(&self);

    /// Stores an opaque caller cookie on this handle.
    fn set_cookie(&self, cookie: usize);

    /// Returns the opaque caller cookie.
    fn cookie(&self) -> usize;

    /// Returns whether this handle may be attached to a TIPC message.
    fn is_sendable(&self) -> bool {
        true
    }

    /// Exposes the concrete value for checked downcasting.
    fn as_any(&self) -> &dyn Any;
}

/// Shared event notification and cookie storage embedded by TIPC objects.
pub struct HandleWaitState {
    poll_set: PollSet,
    cookie: AtomicUsize,
}

impl HandleWaitState {
    /// Creates an empty wait state.
    pub const fn new() -> Self {
        Self {
            poll_set: PollSet::new(),
            cookie: AtomicUsize::new(0),
        }
    }

    /// Registers the current task waker for a future event.
    pub fn register(&self, cx: &mut Context<'_>) {
        self.poll_set.register(cx.waker());
    }

    /// Wakes all tasks registered on this handle.
    pub fn notify(&self) {
        self.poll_set.wake();
    }

    /// Stores an opaque caller cookie.
    pub fn set_cookie(&self, cookie: usize) {
        self.cookie.store(cookie, Ordering::Release);
    }

    /// Loads the opaque caller cookie.
    pub fn cookie(&self) -> usize {
        self.cookie.load(Ordering::Acquire)
    }
}

impl Default for HandleWaitState {
    fn default() -> Self {
        Self::new()
    }
}
