// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ waiter notification owned by the generic IRQ core.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use kpoll::{PollContext, PollRegisterError, PollSet};
use kspin::SpinNoIrq;

use crate::{IRQ_EVENT_SOURCES, IrqEventSource, Virq};

struct IrqPollSets {
    irq: Virq,
    line: Option<PollSet>,
    sources: [Option<PollSet>; IRQ_EVENT_SOURCES],
}

impl IrqPollSets {
    const fn new(irq: Virq) -> Self {
        Self {
            irq,
            line: None,
            sources: [const { None }; IRQ_EVENT_SOURCES],
        }
    }
}

static IRQ_WAITERS: SpinNoIrq<Vec<IrqPollSets>> = SpinNoIrq::new(Vec::new());
static IRQ_WAITER_ENTRIES: AtomicUsize = AtomicUsize::new(0);

fn store_waiter_entry_count(count: usize) {
    IRQ_WAITER_ENTRIES.store(count, Ordering::Release);
}

fn waiter_entries_may_exist() -> bool {
    IRQ_WAITER_ENTRIES.load(Ordering::Acquire) != 0
}

fn find_irq_index(waiters: &[IrqPollSets], irq: Virq) -> Result<usize, usize> {
    waiters.binary_search_by_key(&irq, |waiter| waiter.irq)
}

fn lookup_or_insert_irq_index(
    waiters: &mut Vec<IrqPollSets>,
    irq: Virq,
) -> Result<(usize, bool), PollRegisterError> {
    match find_irq_index(waiters, irq) {
        Ok(index) => Ok((index, false)),
        Err(index) => {
            waiters
                .try_reserve(1)
                .map_err(|_| PollRegisterError::NoMemory)?;
            waiters.insert(index, IrqPollSets::new(irq));
            Ok((index, true))
        }
    }
}

fn lookup_or_insert_line(irq: Virq) -> Result<PollSet, PollRegisterError> {
    {
        let waiters = IRQ_WAITERS.lock();
        if let Some(poll_set) = find_irq_index(&waiters, irq)
            .ok()
            .and_then(|index| waiters[index].line.as_ref())
        {
            return Ok(poll_set.clone());
        }
    }

    // Allocate the `PollSet` outside `IRQ_WAITERS`; cloning/waker storage may grow.
    let poll_set = PollSet::new();
    let mut waiters = IRQ_WAITERS.lock();
    let (index, inserted) = lookup_or_insert_irq_index(&mut waiters, irq)?;
    if let Some(existing) = waiters[index].line.as_ref() {
        return Ok(existing.clone());
    }
    waiters[index].line = Some(poll_set.clone());
    if inserted {
        store_waiter_entry_count(waiters.len());
    }
    Ok(poll_set)
}

fn lookup_or_insert_source(
    irq: Virq,
    source: IrqEventSource,
) -> Result<PollSet, PollRegisterError> {
    if source >= IRQ_EVENT_SOURCES as u8 {
        return Err(PollRegisterError::InvalidState);
    }
    let source_index = source as usize;

    {
        let waiters = IRQ_WAITERS.lock();
        if let Some(poll_set) = find_irq_index(&waiters, irq)
            .ok()
            .and_then(|index| waiters[index].sources[source_index].as_ref())
        {
            return Ok(poll_set.clone());
        }
    }

    // Allocate the `PollSet` outside `IRQ_WAITERS`; cloning/waker storage may grow.
    let poll_set = PollSet::new();
    let mut waiters = IRQ_WAITERS.lock();
    let (index, inserted) = lookup_or_insert_irq_index(&mut waiters, irq)?;
    if let Some(existing) = waiters[index].sources[source_index].as_ref() {
        return Ok(existing.clone());
    }
    waiters[index].sources[source_index] = Some(poll_set.clone());
    if inserted {
        store_waiter_entry_count(waiters.len());
    }
    Ok(poll_set)
}

pub(super) fn remove_irq_waiters(irq: Virq) -> bool {
    let removed = {
        let mut waiters = IRQ_WAITERS.lock();
        let removed = find_irq_index(&waiters, irq)
            .ok()
            .map(|index| waiters.remove(index));
        if removed.is_some() {
            store_waiter_entry_count(waiters.len());
        }
        removed
    };
    let was_removed = removed.is_some();
    // Dropping the table-owned `PollSet`s releases the last strong wake-source
    // owners for detached IRQ waiters. `PollSetInner::drop` wakes those waiters;
    // keep that destructor outside `IRQ_WAITERS`.
    drop(removed);
    was_removed
}

#[cfg(unittest)]
pub(super) fn has_irq_waiters_for_tests(irq: Virq) -> bool {
    find_irq_index(&IRQ_WAITERS.lock(), irq).is_ok()
}

#[cfg(unittest)]
pub(super) fn waiter_entry_count_for_tests() -> usize {
    IRQ_WAITER_ENTRIES.load(Ordering::Acquire)
}

fn collect_wake_sets(
    irq: Virq,
    sources: u8,
) -> (Option<PollSet>, [Option<PollSet>; IRQ_EVENT_SOURCES]) {
    let mut source_sets: [Option<PollSet>; IRQ_EVENT_SOURCES] = [const { None }; IRQ_EVENT_SOURCES];
    if !waiter_entries_may_exist() {
        return (None, source_sets);
    }
    let line_set = {
        let waiters = IRQ_WAITERS.lock();
        let Ok(index) = find_irq_index(&waiters, irq) else {
            return (None, source_sets);
        };
        let waiter = &waiters[index];
        for (source, source_set) in source_sets.iter_mut().enumerate() {
            if sources & (1u8 << source) != 0 {
                *source_set = waiter.sources[source].clone();
            }
        }
        waiter.line.clone()
    };
    (line_set, source_sets)
}

fn wake_collected_sets(
    line_set: Option<PollSet>,
    source_sets: [Option<PollSet>; IRQ_EVENT_SOURCES],
) {
    if let Some(set) = line_set {
        set.wake();
    }
    for set in source_sets.into_iter().flatten() {
        set.wake();
    }
}

/// Registers the current wait for any claimed event on an IRQ line.
///
/// # Errors
///
/// Returns an error when the waiter registration cannot be retained.
pub fn register_irq_waker(
    irq: Virq,
    context: &mut PollContext<'_>,
) -> Result<(), PollRegisterError> {
    let poll_set = lookup_or_insert_line(irq)?;
    // Register outside the waiter-table lock so `Waker::clone` / spill growth
    // cannot run under `SpinNoIrq`.
    context.register(&poll_set)
}

/// Registers the current wait for a specific logical source on an IRQ line.
///
/// # Errors
///
/// Returns an error when `source` is outside the supported event bitmap or when
/// the waiter registration cannot be retained.
pub fn register_irq_source_waker(
    irq: Virq,
    source: IrqEventSource,
    context: &mut PollContext<'_>,
) -> Result<(), PollRegisterError> {
    let poll_set = lookup_or_insert_source(irq, source)?;
    // Register outside the waiter-table lock so `Waker::clone` / spill growth
    // cannot run under `SpinNoIrq`.
    context.register(&poll_set)
}

/// Wake line and logical source waiters for one completed IRQ event.
///
/// Called by the generic IRQ dispatch path after handler fanout has completed.
/// Line waiters are woken before source waiters. Matching [`PollSet`]s are
/// cloned under one waiter-table lock and woken after the lock is released.
pub(super) fn dispatch_irq_event_waiters(irq: Virq, sources: u8) {
    let (line_set, source_sets) = collect_wake_sets(irq, sources);
    wake_collected_sets(line_set, source_sets);
}
