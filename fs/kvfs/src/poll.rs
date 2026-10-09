// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readiness-first waits for explicitly opted-in file operations.

use kerrno::{KError, KResult};
use kpoll::{IoEvents, Pollable};
use ktask::future::poll_io_with_cause;

/// Tries file I/O, waiting only when the operation would block.
///
/// Nonblocking calls never register a wait. A signal interrupting a blocking
/// wait triggers one final operation attempt: readiness, EOF and other errors
/// take precedence over [`KError::IoWaitInterrupted`]. Ordinary operation and
/// registration errors are never promoted to that marker.
///
/// Poll in sleepable task context. `operation_fn` may acquire sleepable object
/// locks, but must return `WouldBlock` instead of waiting for I/O readiness and
/// release its locks before returning. It owns any partial progress: an enclosing
/// transfer must report completed bytes before propagating an interruption. The marker
/// defaults to EINTR; only audited syscall exits may request syscall restart.
pub async fn poll_file_io<P: Pollable, F: FnMut() -> KResult<T>, T>(
    pollable: &P,
    events: IoEvents,
    is_nonblocking: bool,
    mut operation_fn: F,
) -> KResult<T> {
    match operation_fn() {
        Err(KError::WouldBlock) if !is_nonblocking => {}
        result => return result,
    }

    match poll_io_with_cause(pollable, events, false, &mut operation_fn).await {
        Ok(result) => result,
        Err(interrupted) if interrupted.is_signal() => match operation_fn() {
            Err(KError::WouldBlock) => Err(KError::IoWaitInterrupted),
            result => result,
        },
        Err(error) => Err(error.into()),
    }
}

#[unittest::mod_test]
mod tests {
    use alloc::{boxed::Box, sync::Arc, task::Wake};
    use core::{
        cell::Cell,
        future::Future,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };

    use kpoll::{PollContext, PollRegisterError, PollSet};
    use unittest::def_test;

    use super::*;

    struct Input {
        value: Cell<usize>,
        events: PollSet,
        publish_on_register: Cell<bool>,
    }

    impl Input {
        fn new() -> Self {
            Self {
                value: Cell::new(0),
                events: PollSet::new(),
                publish_on_register: Cell::new(false),
            }
        }

        fn read(&self) -> KResult<usize> {
            match self.value.replace(0) {
                0 => Err(KError::WouldBlock),
                value => Ok(value),
            }
        }
    }

    impl Pollable for Input {
        fn poll(&self) -> IoEvents {
            if self.value.get() == 0 {
                IoEvents::empty()
            } else {
                IoEvents::IN
            }
        }

        fn register(
            &self,
            context: &mut PollContext<'_>,
            _: IoEvents,
        ) -> Result<(), PollRegisterError> {
            context.register(&self.events)?;
            if self.publish_on_register.replace(false) {
                self.value.set(11);
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct InterruptCleanup;
    impl Drop for InterruptCleanup {
        fn drop(&mut self) {
            ktask::current().clear_interrupt();
        }
    }

    #[def_test]
    fn file_wait_rechecks_readiness_published_while_registering() {
        let _cleanup = InterruptCleanup;
        ktask::current().clear_interrupt();
        let input = Input::new();
        input.publish_on_register.set(true);
        let mut wait = Box::pin(poll_file_io(&input, IoEvents::IN, false, || input.read()));
        let mut cx = Context::from_waker(Waker::noop());
        // No wake is sent: only the post-registration recheck can see the data.
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(11)));
        assert_eq!(input.value.get(), 0);
    }

    #[def_test]
    fn file_wait_rechecks_after_signal_and_releases_registrations() {
        let _cleanup = InterruptCleanup;
        ktask::current().clear_interrupt();
        let input = Input::new();
        let wakes = Arc::new(WakeCount::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut wait = Box::pin(poll_file_io(&input, IoEvents::IN, false, || input.read()));
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending);
        input.value.set(7);
        input.events.wake();
        assert!(wakes.0.load(Ordering::Relaxed) > 0);
        ktask::current().interrupt();
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(Ok(7)));
        drop(wait);
        assert_eq!(input.value.get(), 0);
        wakes.0.store(0, Ordering::Relaxed);
        input.events.wake();
        ktask::current().interrupt();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);

        ktask::current().clear_interrupt();
        let mut wait = Box::pin(poll_file_io(&input, IoEvents::IN, false, || input.read()));
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending);
        drop(wait);
        wakes.0.store(0, Ordering::Relaxed);
        input.events.wake();
        ktask::current().interrupt();
        assert_eq!(wakes.0.load(Ordering::Relaxed), 0);
    }

    #[def_test]
    fn file_wait_marks_only_signal_interruption_and_keeps_old_poll_order() {
        let _cleanup = InterruptCleanup;
        let input = Input::new();
        let mut cx = Context::from_waker(Waker::noop());
        ktask::current().clear_interrupt();
        let mut wait = Box::pin(poll_file_io(&input, IoEvents::IN, false, || input.read()));
        assert_eq!(wait.as_mut().poll(&mut cx), Poll::Pending);
        ktask::current().interrupt();
        assert_eq!(
            wait.as_mut().poll(&mut cx),
            Poll::Ready(Err(KError::IoWaitInterrupted))
        );
        drop(wait);

        ktask::current().interrupt();
        assert_eq!(
            ktask::future::block_on(poll_file_io(&input, IoEvents::IN, true, || input.read())),
            Err(KError::WouldBlock)
        );
        assert!(ktask::current().is_interrupted());
        input.value.set(9);
        assert_eq!(
            ktask::future::block_on(
                ktask::future::poll_io(&input, IoEvents::IN, true, || input.read())
            ),
            Err(KError::Interrupted)
        );
        assert_eq!(input.value.get(), 9);

        let result: KResult<usize> =
            ktask::future::block_on(poll_file_io(&input, IoEvents::IN, false, || {
                Err(KError::Interrupted)
            }));
        assert_eq!(result, Err(KError::Interrupted));
    }

    #[def_test]
    fn file_wait_preserves_io_registration_errors() {
        struct InvalidPoll;
        impl Pollable for InvalidPoll {
            fn poll(&self) -> IoEvents {
                IoEvents::empty()
            }

            fn register(
                &self,
                _: &mut PollContext<'_>,
                _: IoEvents,
            ) -> Result<(), PollRegisterError> {
                Err(PollRegisterError::InvalidState)
            }
        }
        let _cleanup = InterruptCleanup;
        ktask::current().clear_interrupt();
        let result: KResult<usize> =
            ktask::future::block_on(poll_file_io(&InvalidPoll, IoEvents::IN, false, || {
                Err(KError::WouldBlock)
            }));
        assert_eq!(result, Err(KError::InvalidInput));
    }
}
