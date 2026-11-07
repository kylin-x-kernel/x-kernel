// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Byte-range invalidation tests.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::KError;
use ktask::WaitQueue;
use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{new_cache, write_cached};
use crate::MappingPages;

fn add_clean(cache: &MappingPages, index: u64) {
    cache
        .filemap_add_folio(super::folio_from_bytes(index, b"clean"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
}

fn resident_indices(cache: &MappingPages) -> Vec<u64> {
    cache.state.lock().entries.keys().copied().collect()
}

#[def_test]
fn invalidate_discards_clean_folios_only_inside_range() {
    let cache = new_cache();
    for index in 0..4 {
        add_clean(&cache, index);
    }

    cache
        .invalidate_pages_range(PAGE_SIZE_4K as u64, (PAGE_SIZE_4K * 3) as u64)
        .unwrap();

    assert_eq!(resident_indices(&cache), vec![0, 3]);
    assert_eq!(cache.nrpages(), 2);
}

#[def_test]
fn invalidate_keeps_dirty_folios_and_reports_busy() {
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();
    add_clean(&cache, 1);

    let result = cache.invalidate_pages_range(0, (PAGE_SIZE_4K * 2) as u64);

    assert_eq!(result, Err(KError::ResourceBusy));
    assert_eq!(resident_indices(&cache), vec![0]);
    assert_eq!(
        cache
            .state
            .lock()
            .dirty_marks
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![0]
    );
}

#[def_test]
fn invalidate_rounds_partial_page_bounds_outward() {
    let cache = new_cache();
    for index in 0..3 {
        add_clean(&cache, index);
    }

    cache.invalidate_pages_range(1, PAGE_SIZE_4K as u64 + 1).unwrap();

    assert_eq!(resident_indices(&cache), vec![2]);
}

#[def_test]
fn invalidate_ignores_empty_and_inverted_ranges() {
    let cache = new_cache();
    for index in 0..2 {
        add_clean(&cache, index);
    }

    cache.invalidate_pages_range(0, 0).unwrap();
    cache
        .invalidate_pages_range(PAGE_SIZE_4K as u64, 0)
        .unwrap();
    cache
        .invalidate_pages_range((PAGE_SIZE_4K * 2) as u64, (PAGE_SIZE_4K * 2) as u64)
        .unwrap();

    assert_eq!(cache.nrpages(), 2);
}

#[def_test]
fn invalidate_ignores_unaligned_empty_and_inverted_ranges() {
    let page = PAGE_SIZE_4K as u64;
    // Each case is empty or inverted and lies inside folio 0, which outward
    // rounding would otherwise target.
    let cases = [(1, 1), (page - 1, page - 1), (2, 1), (page - 2, page - 3)];

    let cache = new_cache();
    add_clean(&cache, 0);
    for (start, end) in cases {
        assert_eq!(cache.invalidate_pages_range(start, end), Ok(()));
    }
    assert_eq!(resident_indices(&cache), vec![0]);

    // A dirty folio must not turn the no-op into ResourceBusy.
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();
    for (start, end) in cases {
        assert_eq!(cache.invalidate_pages_range(start, end), Ok(()));
    }
    assert_eq!(resident_indices(&cache), vec![0]);
}

#[def_test]
fn invalidate_scans_every_batch_up_to_the_end_bound() {
    let cache = new_cache();
    let count = crate::FolioBatch::CAPACITY * 3 + 1;
    for page in 0..count as u64 {
        add_clean(&cache, page * 2);
    }

    let end = (count as u64) * 2 * PAGE_SIZE_4K as u64;
    cache.invalidate_pages_range(0, end).unwrap();

    assert_eq!(cache.nrpages(), 0);
}

#[def_test]
fn invalidate_waits_for_inflight_writeback_then_discards() {
    let cache = Arc::new(new_cache());
    write_cached(&cache, 0, b"dirty").unwrap();

    let entered = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(WaitQueue::new());
    let mut waiter = None;
    let mut nr_to_write = 1;
    let mut pages_skipped = 0;

    cache
        .writeback_until(
            PAGE_SIZE_4K as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |_, _, _| {
                let waiting_cache = cache.clone();
                let waiting_entered = entered.clone();
                let waiting_finished = finished.clone();
                let waiting_ready = ready.clone();
                waiter = Some(ktask::spawn(move || {
                    waiting_entered.store(true, Ordering::Release);
                    waiting_ready.notify_all(false);
                    waiting_cache
                        .invalidate_pages_range(0, PAGE_SIZE_4K as u64)
                        .unwrap();
                    waiting_finished.store(true, Ordering::Release);
                }));
                ready.wait_until(|| entered.load(Ordering::Acquire));
                assert!(!finished.load(Ordering::Acquire));
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(waiter.expect("invalidate waiter").join(), 0);
    assert!(finished.load(Ordering::Acquire));
    assert_eq!(cache.nrpages(), 0);
}

#[def_test]
fn invalidate_leaves_a_replacement_installed_during_the_scan() {
    let cache = Arc::new(new_cache());
    write_cached(&cache, 0, b"old").unwrap();

    let candidate = cache.state.lock().entries[&0].clone();
    let waiting = Arc::new(AtomicBool::new(false));
    let events = Arc::new(WaitQueue::new());
    let _observation =
        super::WritebackWaitObservation::new(candidate, waiting.clone(), events.clone());

    let mut waiter = None;
    let mut replacement = None;
    let mut nr_to_write = 1;
    let mut pages_skipped = 0;

    cache
        .writeback_until(
            PAGE_SIZE_4K as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |_, _, _| {
                let waiting_cache = cache.clone();
                waiter = Some(ktask::spawn(move || {
                    waiting_cache
                        .invalidate_pages_range(0, PAGE_SIZE_4K as u64)
                        .unwrap();
                }));
                // The scan has collected the candidate and is waiting for this
                // writeback; swap the resident object behind it.
                events.wait_until(|| waiting.load(Ordering::Acquire));
                let fresh = Arc::new(super::folio_from_bytes(0, b"new"));
                let mut state = cache.state.lock();
                state.writeback_marks.remove(&0);
                state.entries.insert(0, fresh.clone());
                drop(state);
                replacement = Some(fresh);
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(waiter.expect("invalidate waiter").join(), 0);
    assert_eq!(resident_indices(&cache), vec![0]);
    let replacement = replacement.expect("replacement installed");
    let is_replacement = cache
        .state
        .lock()
        .entries
        .get(&0)
        .is_some_and(|folio| Arc::ptr_eq(folio, &replacement));
    assert!(is_replacement);
}
