// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resize and final teardown tests.

use alloc::vec;

use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};

#[def_test]
fn shrink_drops_full_folios_and_zeros_surviving_tail() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![0x5a; PAGE_SIZE_4K * 2]).unwrap();

    let new_len = (PAGE_SIZE_4K / 2) as u64;
    cache.resize_cached_folios((PAGE_SIZE_4K * 2) as u64, new_len);

    assert_eq!(cache.nrpages(), 1);
    assert_eq!(dirty_indices(&cache), vec![0]);
    let tail_is_zero = cache.with_folio(0, |folio| {
        let folio = folio.expect("surviving folio");
        let tail_is_zero = folio.data()[PAGE_SIZE_4K / 2..]
            .iter()
            .all(|byte| *byte == 0);
        folio.clear_dirty();
        tail_is_zero
    });
    assert!(tail_is_zero);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn growth_zeros_cached_tail_and_marks_it_dirty() {
    let cache = new_cache();
    cache
        .filemap_add_folio(super::folio_from_bytes(0, &vec![0x5a; PAGE_SIZE_4K]))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    let old_len = (PAGE_SIZE_4K / 4) as u64;
    let new_len = (PAGE_SIZE_4K / 2) as u64;
    assert!(dirty_indices(&cache).is_empty());

    cache.resize_cached_folios(old_len, new_len);

    assert_eq!(dirty_indices(&cache), vec![0]);
    let (is_dirty, growth_range_is_zero) = cache.with_folio(0, |folio| {
        let folio = folio.expect("grown tail folio");
        let state = (
            folio.is_dirty(),
            folio.data()[old_len as usize..new_len as usize]
                .iter()
                .all(|byte| *byte == 0),
        );
        folio.clear_dirty();
        state
    });
    assert!(is_dirty);
    assert!(growth_range_is_zero);
    assert!(dirty_indices(&cache).is_empty());
}
#[def_test]
fn final_truncate_discards_dirty_folios() {
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();

    cache.truncate_final().unwrap();
    assert_eq!(cache.nrpages(), 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn shrink_and_final_teardown_remove_multiple_sparse_batches() {
    let cache = new_cache();
    let count = crate::FolioBatch::CAPACITY * 3 + 2;
    for index in 0..count {
        write_cached(&cache, (index * 2 * PAGE_SIZE_4K) as u64, b"dirty").unwrap();
    }
    // Teardown uses page indices directly and must handle the maximum index
    // without wrapping a traversal cursor back to zero.
    cache
        .filemap_add_folio(super::folio_from_bytes(u64::MAX, b"last"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    let retained = cache.state.lock().entries[&((count - 1) as u64 * 2)]
        .clone();
    let new_len = (crate::FolioBatch::CAPACITY * 2 * PAGE_SIZE_4K + 1) as u64;
    cache.resize_cached_folios((count * 2 * PAGE_SIZE_4K) as u64, new_len);
    assert_eq!(cache.nrpages(), (crate::FolioBatch::CAPACITY + 1) as u64);
    assert!(!retained.lock().is_dirty());
    cache.with_folio((crate::FolioBatch::CAPACITY * 2) as u64, |folio| {
        assert!(folio.unwrap().data()[1..].iter().all(|byte| *byte == 0));
    });
    cache.truncate_final().unwrap();
    assert_eq!(cache.nrpages(), 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn mapping_drop_cleans_retained_folios_across_batches() {
    let retained = {
        let cache = new_cache();
        let count = crate::FolioBatch::CAPACITY * 2 + 1;
        write_cached(&cache, 0, &vec![0x5a; count * PAGE_SIZE_4K]).unwrap();
        let retained = cache.state.lock().entries[&(crate::FolioBatch::CAPACITY as u64)]
            .clone();
        assert!(retained.lock().is_dirty());
        retained
    };
    let folio = retained.lock();
    assert_eq!(folio.index(), crate::FolioBatch::CAPACITY as u64);
    assert!(!folio.is_dirty());
    assert!(!folio.is_under_writeback());
}

#[def_test]
fn truncate_waits_for_writeback_after_removing_ready_folios() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use kerrno::KError;
    use ktask::WaitQueue;

    // Run I/O and truncation on separate tasks. The controller releases I/O
    // after observing first-pass progress, never after waiting for truncation.
    for mode in 0..4 {
        let cache = Arc::new(new_cache());
        write_cached(&cache, 0, &vec![0x5a; PAGE_SIZE_4K * 2]).unwrap();
        let old_folio = cache.state.lock().entries[&0].clone();
        let events = Arc::new(WaitQueue::new());
        let io_started = Arc::new(AtomicBool::new(false));
        let release_io = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let wait_entered = Arc::new(AtomicBool::new(false));
        let _observation = super::WritebackWaitObservation::new(
            old_folio.clone(),
            wait_entered.clone(),
            events.clone(),
        );
        let writing_cache = cache.clone();
        let writing_events = events.clone();
        let writing_started = io_started.clone();
        let writing_release = release_io.clone();
        let writer = ktask::spawn(move || {
            let mut budget = 1;
            let mut skipped = 0;
            let result = writing_cache.write_cache_pages(
                (PAGE_SIZE_4K * 2) as u64,
                0..PAGE_SIZE_4K as u64,
                &mut budget,
                &mut skipped,
                PAGE_SIZE_4K,
                &mut |_, _| {
                    writing_started.store(true, Ordering::Release);
                    writing_events.notify_all(false);
                    writing_events.wait_until(|| writing_release.load(Ordering::Acquire));
                    Err((0, KError::InvalidInput))
                },
            );
            core::assert_eq!(result.err(), Some(KError::InvalidInput));
        });
        events.wait_until(|| io_started.load(Ordering::Acquire));
        let truncating_cache = cache.clone();
        let truncating_finished = finished.clone();
        let truncating_events = events.clone();
        let truncator = ktask::spawn(move || {
            if mode == 0 {
                truncating_cache.truncate_final().unwrap();
            } else {
                truncating_cache.resize_cached_folios(
                    (PAGE_SIZE_4K * 2) as u64,
                    if mode == 1 {
                        0
                    } else {
                        (PAGE_SIZE_4K / 2) as u64
                    },
                );
            }
            truncating_finished.store(true, Ordering::Release);
            truncating_events.notify_all(false);
        });
        // Observe the actual writeback wait, not task entry or elapsed time.
        // If truncation returns without waiting, finished also wakes us so the
        // missing wait fails explicitly instead of stranding the controller.
        events.wait_until(|| {
            wait_entered.load(Ordering::Acquire) || finished.load(Ordering::Acquire)
        });
        let entered_writeback_wait = wait_entered.load(Ordering::Acquire);
        let finished_before_io = finished.load(Ordering::Acquire);
        let removed_ready_folio = !cache
            .state
            .try_lock()
            .expect("writeback waiter must release mapping state lock")
            .entries
            .contains_key(&1);
        let tail_unchanged = old_folio.lock().data().iter().all(|byte| *byte == 0x5a);
        if !entered_writeback_wait || !removed_ready_folio || finished_before_io || !tail_unchanged
        {
            // Unblock both workers before reporting a broken phase contract.
            release_io.store(true, Ordering::Release);
            events.notify_all(false);
            writer.join();
            truncator.join();
        }
        core::assert!(
            entered_writeback_wait,
            "truncation did not wait for writeback"
        );
        core::assert!(
            removed_ready_folio,
            "first pass did not remove the ready folio"
        );
        core::assert!(!finished_before_io, "truncation completed before I/O");
        core::assert!(tail_unchanged, "tail was changed before I/O completed");
        if mode == 3 {
            let mut replacement = crate::Folio::new_zeroed(0).unwrap();
            replacement.data().fill(0x33);
            replacement.mark_uptodate();
            replacement.mark_dirty();
            let mut state = cache.state.lock();
            state.entries.insert(
                0,
                Arc::new(ksync::Mutex::new(
                    replacement,
                )),
            );
            state.dirty_marks.insert(0);
            state.writeback_marks.remove(&0);
        } else {
            cache.with_folio(0, |folio| folio.unwrap().mark_dirty());
        }
        release_io.store(true, Ordering::Release);
        events.notify_all(false);
        events.wait_until(|| finished.load(Ordering::Acquire));
        assert_eq!(writer.join(), 0);
        assert_eq!(truncator.join(), 0);
        assert!(cache.state.lock().writeback_marks.is_empty());
        if mode < 2 {
            assert_eq!(cache.nrpages(), 0);
            assert!(!old_folio.lock().is_dirty());
        } else {
            assert_eq!(cache.nrpages(), 1);
            cache.with_folio(0, |folio| {
                let folio = folio.unwrap();
                core::assert!(folio.is_dirty());
                core::assert!(
                    folio.data()[PAGE_SIZE_4K / 2..]
                        .iter()
                        .all(|byte| *byte == 0)
                );
                core::assert_eq!(folio.data()[0], if mode == 3 { 0x33 } else { 0x5a });
            });
            if mode == 3 {
                assert!(!old_folio.lock().is_dirty());
            }
            cache.truncate_final().unwrap();
        }
    }
}

#[def_test]
fn growth_during_batch_writeback_keeps_new_bytes_dirty() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![0x5a; PAGE_SIZE_4K]).unwrap();
    let old_len = (PAGE_SIZE_4K / 4) as u64;
    let new_len = (PAGE_SIZE_4K / 2) as u64;
    let mut budget = 1;
    let mut skipped = 0;
    cache
        .write_cache_pages(
            old_len,
            0..PAGE_SIZE_4K as u64,
            &mut budget,
            &mut skipped,
            PAGE_SIZE_4K,
            &mut |_, data| {
                // Growth must finish while this batch still owns writeback. Its
                // new dirty bytes must survive completion of the older snapshot.
                cache.resize_cached_folios(old_len, new_len);
                core::assert!(data.iter().all(|byte| *byte == 0x5a));
                Ok(data.len())
            },
        )
        .unwrap();
    cache.with_folio(0, |folio| {
        let folio = folio.unwrap();
        core::assert!(folio.is_dirty());
        core::assert!(!folio.is_under_writeback());
        core::assert!(
            folio.data()[old_len as usize..new_len as usize]
                .iter()
                .all(|byte| *byte == 0)
        );
    });
    assert_eq!(dirty_indices(&cache), vec![0]);
    cache.truncate_final().unwrap();
}
