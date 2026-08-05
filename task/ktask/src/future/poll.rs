// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async helpers built on top of pollable I/O and IRQ wakers.

use alloc::collections::{BTreeMap, btree_map::Entry};
use core::{future::poll_fn, task::Poll};

use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollRegistrations, PollSet, Pollable};
use kspin::SpinNoIrq;

/// A helper to wrap a synchronous non-blocking I/O function into an
/// asynchronous function.
///
/// # Arguments
///
/// * `pollable`: The pollable object to register for I/O events.
/// * `events`: The I/O events to wait for.
/// * `non_blocking`: If true, the function will return `KError::WouldBlock`
///   immediately when the I/O operation would block.
/// * `f`: The synchronous non-blocking I/O function to be wrapped. It should
///   return `KError::WouldBlock` when the operation would block.
pub async fn poll_io<P: Pollable, F: FnMut() -> KResult<T>, T>(
    pollable: &P,
    events: IoEvents,
    non_blocking: bool,
    mut f: F,
) -> KResult<T> {
    let mut registrations = PollRegistrations::new();
    super::interruptible(poll_fn(move |cx| match f() {
        Ok(value) => Poll::Ready(Ok(value)),
        Err(KError::WouldBlock) => {
            if non_blocking {
                return Poll::Ready(Err(KError::WouldBlock));
            }
            let mut context = registrations.context(cx);
            if let Err(error) = pollable.register(&mut context, events) {
                return Poll::Ready(Err(map_register_error(error)));
            }
            drop(context);
            match f() {
                Ok(value) => Poll::Ready(Ok(value)),
                Err(KError::WouldBlock) => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            }
        }
        Err(e) => Poll::Ready(Err(e)),
    }))
    .await?
}

fn map_register_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}

/// Registers a waker for the given IRQ number.
pub fn register_irq_waker(
    irq: usize,
    context: &mut PollContext<'_>,
) -> Result<(), PollRegisterError> {
    static POLL_IRQ: SpinNoIrq<BTreeMap<usize, PollSet>> = SpinNoIrq::new(BTreeMap::new());

    fn irq_hook(irq: usize) {
        let set = { POLL_IRQ.lock().get(&irq).cloned() };
        if let Some(set) = set {
            set.wake();
        }
    }

    let set = {
        let sets = POLL_IRQ.lock();
        if let Some(existing) = sets.get(&irq) {
            existing.clone()
        } else {
            drop(sets);
            let set = PollSet::new();
            let mut sets = POLL_IRQ.lock();
            match sets.entry(irq) {
                Entry::Vacant(entry) => {
                    assert!(
                        kirq::subscribe_wakeup(irq, irq_hook),
                        "failed to subscribe IRQ wakeup for irq={irq}"
                    );
                    entry.insert(set).clone()
                }
                Entry::Occupied(entry) => entry.get().clone(),
            }
        }
    };
    context.register(&set)
}
