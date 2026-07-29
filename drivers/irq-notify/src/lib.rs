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
use core::task::Waker;

use device_res::{IRQ_EVENT_SOURCES, IrqEventSource};
use kpoll::{PollContext, PollRegisterError, PollRegistration, PollSet};
use kspin::SpinNoIrq;

struct IrqSourcePollSet {
    irq: usize,
    source: IrqEventSource,
    poll_set: PollSet,
    /// Long-lived bridge registration used by device layers that still hand a
    /// derived [`Waker`] rather than a caller-owned [`PollContext`].
    bridge: Option<PollRegistration>,
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
        bridge: None,
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

/// Installs a long-lived bridge waker for a device IRQ source.
///
/// Device backends that only receive a derived [`Waker`] use this entry point.
/// The previous bridge registration, if any, is replaced.
///
/// # Errors
///
/// Returns an error when the waiter registration cannot be retained.
pub fn bind_source_waker(
    irq: usize,
    source: IrqEventSource,
    waker: &Waker,
) -> Result<(), PollRegisterError> {
    let poll_set = lookup_or_insert(irq, source)?;
    // Register outside the table lock, then publish the bridge guard under it.
    let registration = poll_set.register(waker)?;
    {
        let mut waiters = IRQ_SOURCE_WAITERS.lock();
        let Some(index) = find_index(&waiters, irq, source) else {
            // Entry removed underfoot; keep the registration alive via drop.
            return Ok(());
        };
        waiters[index].bridge = Some(registration);
    }
    // Close the publish/IRQ race: an interrupt between register and publish
    // would not yet see this bridge as the device layer's retained waiter, and
    // one-shot wake may already have detached it. Prompt the bound task.
    waker.wake_by_ref();
    Ok(())
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
