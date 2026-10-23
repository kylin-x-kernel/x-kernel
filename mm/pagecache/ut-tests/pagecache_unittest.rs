// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared fixtures for cache algorithm tests.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::KResult;
use ksync::{Mutex, static_lock};
use ktask::WaitQueue;
use memaddr::PAGE_SIZE_4K;

use crate::{Folio, MappingPages, PageIndex};

#[path = "mapping_pages_unittest.rs"]
mod mapping_pages;
#[path = "residency_unittest.rs"]
mod residency;
#[path = "truncate_unittest.rs"]
mod truncate;
#[path = "writeback_unittest.rs"]
mod writeback;

fn new_cache() -> MappingPages {
    MappingPages::new()
}

fn dirty_indices(cache: &MappingPages) -> Vec<PageIndex> {
    cache.state.lock().dirty_marks.iter().copied().collect()
}

fn write_cached(cache: &MappingPages, offset: u64, src: &[u8]) -> KResult<()> {
    let mut written = 0usize;
    while written < src.len() {
        let pos = offset + written as u64;
        let index = pos / PAGE_SIZE_4K as u64;
        let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
        let step = (src.len() - written).min(PAGE_SIZE_4K - page_off);
        cache.with_folio_or_create(
            index,
            |_| Ok(()),
            |folio| {
                folio.data()[page_off..page_off + step]
                    .copy_from_slice(&src[written..written + step]);
                folio.mark_dirty();
                Ok(())
            },
        )?;
        written += step;
    }
    Ok(())
}

// One scoped observer is installed by the truncate concurrency test. Other
// tests can write back concurrently: notifications match the retained Arc.
type FolioWaitSignal = (Arc<Folio>, Arc<AtomicBool>, Arc<WaitQueue>);
static_lock! {
    static WRITEBACK_WAIT_SIGNAL: Mutex<Option<FolioWaitSignal>> = Mutex::new(None);
}

struct WritebackWaitObservation;

impl WritebackWaitObservation {
    fn new(folio: Arc<Folio>, entered: Arc<AtomicBool>, events: Arc<WaitQueue>) -> Self {
        let mut signal = WRITEBACK_WAIT_SIGNAL.lock();
        assert!(
            signal.is_none(),
            "writeback wait observer already installed"
        );
        *signal = Some((folio, entered, events));
        Self
    }
}

impl Drop for WritebackWaitObservation {
    fn drop(&mut self) {
        WRITEBACK_WAIT_SIGNAL.lock().take();
    }
}

pub(super) fn observe_writeback_wait(folio: &Arc<Folio>) {
    let signal = WRITEBACK_WAIT_SIGNAL.lock();
    if let Some((observed, entered, events)) = signal.as_ref()
        && Arc::ptr_eq(observed, folio)
    {
        entered.store(true, Ordering::Release);
        events.notify_all(false);
    }
}

fn folio_from_bytes(index: u64, data: &[u8]) -> crate::Folio {
    let mut folio = crate::Folio::new_zeroed(index).unwrap();
    folio.data()[..data.len()].copy_from_slice(data);
    folio
}

// The read-coordination test observes contention on its own folio only.
static_lock! {
    static READ_WAIT_SIGNAL: Mutex<Option<FolioWaitSignal>> = Mutex::new(None);
}

pub(super) fn observe_read_wait(folio: &Arc<Folio>) {
    let signal = READ_WAIT_SIGNAL.lock();
    if let Some((observed, entered, events)) = signal.as_ref()
        && Arc::ptr_eq(observed, folio)
    {
        entered.store(true, Ordering::Release);
        events.notify_all(false);
    }
}

// Per-mapping, one-shot gate: only the first allocation pauses. Other test
// mappings and the competing allocation can progress independently.
pub(super) struct AllocationGate {
    claimed: AtomicBool,
    pub(super) entered: AtomicBool,
    pub(super) released: AtomicBool,
    pub(super) events: WaitQueue,
}

impl AllocationGate {
    pub(super) fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            released: AtomicBool::new(false),
            events: WaitQueue::new(),
        }
    }
}

pub(super) fn observe_allocation(mapping: &MappingPages) {
    let gate = mapping.allocation_gate.lock().clone();
    if let Some(gate) = gate
        && !gate.claimed.swap(true, Ordering::AcqRel)
    {
        gate.entered.store(true, Ordering::Release);
        gate.events.notify_all(false);
        gate.events
            .wait_until(|| gate.released.load(Ordering::Acquire));
    }
}
