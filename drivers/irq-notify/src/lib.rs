// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel IRQ source waiter notification.
//!
//! This crate bridges resource-level IRQ source reports to x-kernel task
//! wakers. It is intentionally separate from `device-res`: the resource model
//! stays OS-agnostic, while this crate owns the `PollSet`-based wait/wake
//! mechanism used by x-kernel drivers and subsystems.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use device_res::{IRQ_EVENT_SOURCES, IrqEventSource};
use kpoll::{PollContext, PollRegisterError, PollSet};
use kspin::SpinNoIrq;

struct IrqSourcePollSet {
    irq: usize,
    source: IrqEventSource,
    poll_set: PollSet,
}

static IRQ_SOURCE_WAITERS: SpinNoIrq<Vec<IrqSourcePollSet>> = SpinNoIrq::new(Vec::new());

fn find_index(waiters: &[IrqSourcePollSet], irq: usize, source: IrqEventSource) -> Option<usize> {
    waiters
        .iter()
        .position(|waiter| waiter.irq == irq && waiter.source == source)
}

fn insert_new(
    waiters: &mut Vec<IrqSourcePollSet>,
    irq: usize,
    source: IrqEventSource,
    poll_set: PollSet,
) -> Result<usize, PollRegisterError> {
    waiters
        .try_reserve(1)
        .map_err(|_| PollRegisterError::NoMemory)?;
    waiters.push(IrqSourcePollSet {
        irq,
        source,
        poll_set,
    });
    Ok(waiters.len() - 1)
}

fn lookup_or_insert(irq: usize, source: IrqEventSource) -> Result<PollSet, PollRegisterError> {
    {
        let waiters = IRQ_SOURCE_WAITERS.lock();
        if let Some(index) = find_index(&waiters, irq, source) {
            return Ok(waiters[index].poll_set.clone());
        }
    }

    // Allocate the `PollSet` (and its `Arc`) outside `SpinNoIrq`.
    let poll_set = PollSet::new();
    let mut waiters = IRQ_SOURCE_WAITERS.lock();
    if let Some(index) = find_index(&waiters, irq, source) {
        return Ok(waiters[index].poll_set.clone());
    }
    let index = insert_new(&mut waiters, irq, source, poll_set)?;
    Ok(waiters[index].poll_set.clone())
}

/// Registers the current wait for a specific logical source on an IRQ line.
///
/// # Errors
///
/// Returns an error when the waiter registration cannot be retained.
pub fn register_source_waker(
    irq: usize,
    source: IrqEventSource,
    context: &mut PollContext<'_>,
) -> Result<(), PollRegisterError> {
    let poll_set = lookup_or_insert(irq, source)?;
    // Register outside the waiter-table lock so `Waker::clone` / spill growth
    // cannot run under `SpinNoIrq`.
    context.register(&poll_set)
}

/// Wake waiters whose logical IRQ source bits fired.
///
/// Called from IRQ dispatch. Matching [`PollSet`]s are cloned into a fixed
/// stack buffer under the waiter-table lock, then woken after the lock is
/// released so waker callbacks cannot re-enter that lock. The buffer is sized
/// by [`IRQ_EVENT_SOURCES`], so this path never allocates.
pub fn dispatch_sources(irq: usize, sources: u8) {
    let mut to_wake: [Option<PollSet>; IRQ_EVENT_SOURCES] = [const { None }; IRQ_EVENT_SOURCES];
    {
        let waiters = IRQ_SOURCE_WAITERS.lock();
        for waiter in waiters.iter() {
            if waiter.irq != irq
                || waiter.source >= IRQ_EVENT_SOURCES as u8
                || sources & (1 << waiter.source) == 0
            {
                continue;
            }
            // At most one entry per `(irq, source)`.
            let index = waiter.source as usize;
            if to_wake[index].is_none() {
                to_wake[index] = Some(waiter.poll_set.clone());
            }
        }
    }
    for set in to_wake.into_iter().flatten() {
        set.wake();
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::boxed::Box;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, RawWaker, RawWakerVTable, Waker},
    };

    use kpoll::PollRegistrations;
    use unittest::{assert_eq, def_test};

    use super::{dispatch_sources, register_source_waker};

    const TEST_IRQ: usize = 0x5a5a;
    const TEST_SOURCE: u8 = 0;

    unsafe fn clone_waker(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn wake_waker(data: *const ()) {
        // SAFETY: `make_waker` installs a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn wake_waker_by_ref(data: *const ()) {
        // SAFETY: same invariant as `wake_waker`.
        unsafe { wake_waker(data) };
    }

    unsafe fn drop_waker(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_waker, wake_waker, wake_waker_by_ref, drop_waker);

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: all vtable operations preserve the leaked AtomicUsize pointer.
        unsafe { Waker::from_raw(raw) }
    }

    #[def_test]
    fn source_registration_wakes_current_waiter() {
        let counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let waker = make_waker(counter);
        let cx = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();

        {
            let mut context = registrations.context(&cx);
            register_source_waker(TEST_IRQ, TEST_SOURCE, &mut context).unwrap();
        }
        dispatch_sources(TEST_IRQ, 1 << TEST_SOURCE);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        {
            let mut context = registrations.context(&cx);
            register_source_waker(TEST_IRQ, TEST_SOURCE, &mut context).unwrap();
        }
        dispatch_sources(TEST_IRQ, 1 << TEST_SOURCE);
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[def_test]
    fn source_registration_is_cancelled_by_owner() {
        let counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let waker = make_waker(counter);
        let cx = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();

        {
            let mut context = registrations.context(&cx);
            register_source_waker(TEST_IRQ + 1, TEST_SOURCE, &mut context).unwrap();
        }
        registrations.clear();
        dispatch_sources(TEST_IRQ + 1, 1 << TEST_SOURCE);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }
}
