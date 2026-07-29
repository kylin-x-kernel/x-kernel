// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket wait sets and readiness calculation.

use ::core::task::Waker;
use kpoll::{IoEvents, PollSet};

use super::state::UdpSocketLifecycle;

/// Read, write, error, and hangup wait sets owned by a socket.
pub(crate) struct UdpSocketWaiters {
    read: PollSet,
    write: PollSet,
    error: PollSet,
    hup: PollSet,
}

impl UdpSocketWaiters {
    pub(crate) fn new() -> Self {
        Self {
            read: PollSet::new(),
            write: PollSet::new(),
            error: PollSet::new(),
            hup: PollSet::new(),
        }
    }

    pub(crate) fn register(&self, waker: &Waker, events: IoEvents) {
        if events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND) {
            self.read.register(waker);
        }
        if events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND) {
            self.write.register(waker);
        }
        if events.contains(IoEvents::ERR) {
            self.error.register(waker);
        }
        if events.intersects(IoEvents::HUP | IoEvents::RDHUP) {
            self.hup.register(waker);
        }
    }

    pub(crate) fn wake_read(&self) {
        self.read.wake();
    }

    pub(crate) fn wake_write(&self) {
        self.write.wake();
    }

    pub(crate) fn wake_error(&self) {
        self.error.wake();
    }

    pub(crate) fn wake_hup(&self) {
        self.hup.wake();
    }

    pub(crate) fn readiness(
        &self,
        backend_events: IoEvents,
        lifecycle: UdpSocketLifecycle,
        read_shutdown: bool,
        write_shutdown: bool,
        has_error: bool,
    ) -> IoEvents {
        let mut events = backend_events;
        if read_shutdown || matches!(lifecycle, UdpSocketLifecycle::Closed) {
            events.insert(IoEvents::RDHUP | IoEvents::HUP);
        }
        if write_shutdown {
            events.remove(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND);
        }
        events.set(IoEvents::ERR, has_error);
        events
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable, Waker},
    };

    use unittest::{assert_eq, def_test};

    use super::*;

    fn new_counter() -> &'static AtomicUsize {
        Box::leak(Box::new(AtomicUsize::new(0)))
    }

    /// Clones a test waker whose data points to a leaked counter.
    ///
    /// # Safety
    ///
    /// `data` must be the static `AtomicUsize` pointer installed by
    /// `make_waker`. The allocation is intentionally leaked, so cloning the
    /// pointer requires no reference-count update.
    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: `data` is the waker data pointer installed by
        // `make_waker`, which stores a leaked `AtomicUsize` with static
        // lifetime and atomic interior mutation.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: `data` follows the same contract as in `waker_wake`.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    /// Drops a test waker whose leaked counter has no release operation.
    ///
    /// # Safety
    ///
    /// `data` must follow the same static pointer contract as `waker_clone`.
    unsafe fn waker_drop(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: `raw` is built from `WAKER_VTABLE`, whose callbacks preserve
        // the `RawWaker` data pointer contract for the leaked `AtomicUsize`.
        unsafe { Waker::from_raw(raw) }
    }

    #[def_test]
    fn test_udp_socket_waiters_wake_all_read_waiters() {
        let waiters = UdpSocketWaiters::new();
        let first_counter = new_counter();
        let second_counter = new_counter();
        let first_waker = make_waker(first_counter);
        let second_waker = make_waker(second_counter);

        waiters.register(&first_waker, IoEvents::IN);
        waiters.register(&second_waker, IoEvents::IN);
        waiters.wake_read();

        assert_eq!(first_counter.load(Ordering::SeqCst), 1);
        assert_eq!(second_counter.load(Ordering::SeqCst), 1);

        waiters.wake_read();

        assert_eq!(first_counter.load(Ordering::SeqCst), 1);
        assert_eq!(second_counter.load(Ordering::SeqCst), 1);
    }

    #[def_test]
    fn test_udp_socket_waiters_registers_event_specific_sets() {
        let waiters = UdpSocketWaiters::new();
        let read_counter = new_counter();
        let hup_counter = new_counter();
        let read_waker = make_waker(read_counter);
        let hup_waker = make_waker(hup_counter);

        waiters.register(&read_waker, IoEvents::IN);
        waiters.register(&hup_waker, IoEvents::HUP);
        waiters.wake_hup();

        assert_eq!(read_counter.load(Ordering::SeqCst), 0);
        assert_eq!(hup_counter.load(Ordering::SeqCst), 1);

        waiters.wake_read();

        assert_eq!(read_counter.load(Ordering::SeqCst), 1);
    }
}
