// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Writeback identity, completion and redirty tests.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::KError;
use ksync::Mutex;
use ktask::WaitQueue;
use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};
use crate::Folio;

#[def_test]
fn writeback_uses_owner_supplied_visible_length() {
    let cache = new_cache();
    let visible_len = PAGE_SIZE_4K as u64 + 123;
    write_cached(&cache, 0, &vec![0x33; visible_len as usize]).unwrap();
    let mut writes = Vec::new();

    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .writeback_until(
            visible_len,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |index, _data, valid_len| {
                writes.push((index, valid_len));
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(writes, vec![(0, PAGE_SIZE_4K), (1, 123)]);
    assert!(dirty_indices(&cache).is_empty());
    for index in 0..2 {
        let state = cache.with_folio(index, |folio| {
            let folio = folio.expect("written folio");
            (folio.is_dirty(), folio.is_under_writeback())
        });
        assert_eq!(state, (false, false));
    }
}

#[def_test]
fn failed_writeback_restores_dirty_state() {
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();

    let initial_budget = 1;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    assert_eq!(
        cache.writeback_until(
            5,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |_, _, _| { Err(KError::InvalidInput) }
        ),
        Err(KError::InvalidInput)
    );
    assert_eq!(dirty_indices(&cache), vec![0]);
    let is_dirty = cache.with_folio(0, |folio| {
        let folio = folio.expect("failed-writeback folio");
        let is_dirty = folio.is_dirty();
        folio.clear_dirty();
        is_dirty
    });
    assert!(is_dirty);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn clean_folio_with_stale_dirty_tag_is_not_written() {
    let cache = new_cache();
    cache.filemap_add_folio(0, 0, b"clean").unwrap();
    cache.state.lock().dirty_marks.insert(0);
    let mut writes = 0usize;

    let initial_budget = 1;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .writeback_until(
            PAGE_SIZE_4K as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |_, _, _| {
                writes += 1;
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(writes, 0);
    assert_eq!(initial_budget - nr_to_write, 0);
    assert_eq!(pages_skipped, 0);
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn writeback_until_respects_dirty_index_and_page_budget() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
    assert_eq!(dirty_indices(&cache), vec![0, 1]);

    let mut writes = Vec::new();
    let initial_budget = 1;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .writeback_until(
            (PAGE_SIZE_4K * 2) as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |index, _data, valid_len| {
                writes.push((index, valid_len));
                Ok(())
            },
        )
        .unwrap();

    assert_eq!(initial_budget - nr_to_write, 1);
    assert_eq!(writes, vec![(0, PAGE_SIZE_4K)]);
    assert_eq!(dirty_indices(&cache), vec![1]);
    cache.with_folio(1, |folio| {
        folio.expect("remaining dirty folio").clear_dirty();
    });
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn stale_collected_folio_is_not_written_or_tagged_as_replacement() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
    let cache_during_io = &cache;
    let mut write_offsets = Vec::new();

    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .write_cache_pages(
            (PAGE_SIZE_4K * 2) as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            PAGE_SIZE_4K,
            &mut |offset, data| {
                write_offsets.push(offset);
                if offset == 0 {
                    let replacement = Arc::new(Mutex::new(
                        Folio::new_zeroed(1).map_err(|error| (0, error))?,
                    ));
                    let old = cache_during_io
                        .state
                        .lock()
                        .entries
                        .insert(1, replacement)
                        .expect("collected folio");
                    old.lock().clear_dirty();
                }
                Ok(data.len())
            },
        )
        .unwrap();

    assert_eq!(write_offsets, vec![0]);
    assert!(dirty_indices(&cache).is_empty());
    let replacement_state = cache.with_folio(1, |folio| {
        let folio = folio.expect("replacement folio");
        (folio.is_dirty(), folio.is_under_writeback())
    });
    assert_eq!(replacement_state, (false, false));
}

#[def_test]
fn batched_writeback_failure_restores_dirty_index() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();

    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        (PAGE_SIZE_4K * 2) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        PAGE_SIZE_4K * 2,
        &mut |_, _| {
            core::assert_eq!(
                cache
                    .state
                    .lock()
                    .writeback_marks
                    .iter()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![0, 1]
            );
            Err((0, KError::InvalidInput))
        },
    );

    assert_eq!(outcome.err(), Some(KError::InvalidInput));
    assert_eq!(initial_budget - nr_to_write, 0);
    assert_eq!(dirty_indices(&cache), vec![0, 1]);
    assert!(cache.state.lock().writeback_marks.is_empty());
    for index in 0..2 {
        cache.with_folio(index, |folio| {
            let folio = folio.expect("failed batch folio");
            assert!(folio.is_dirty());
            assert!(!folio.is_under_writeback());
            folio.clear_dirty();
        });
    }
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn failed_writeback_does_not_redirty_detached_folio() {
    let cache = new_cache();
    write_cached(&cache, 0, b"dirty").unwrap();
    let detached = cache
        .state
        .lock()
        .entries
        .get(&0)
        .cloned()
        .expect("cached folio");
    let cache_during_io = &cache;

    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        PAGE_SIZE_4K as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        PAGE_SIZE_4K,
        &mut |_, _| {
            cache_during_io.resize_cached_folios(PAGE_SIZE_4K as u64, 0);
            core::assert!(cache_during_io.state.lock().writeback_marks.is_empty());
            Err((0, KError::InvalidInput))
        },
    );

    assert_eq!(outcome.err(), Some(KError::InvalidInput));
    assert!(dirty_indices(&cache).is_empty());
    let detached_state = {
        let folio = detached.lock();
        (folio.is_dirty(), folio.is_under_writeback())
    };
    assert_eq!(detached_state, (false, false));
}

#[def_test]
fn range_wait_wakes_after_batch_writeback() {
    let cache = Arc::new(new_cache());
    write_cached(&cache, 0, b"old").unwrap();
    let entered = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(WaitQueue::new());
    let mut waiter = None;
    let mut nr_to_write = 1;
    let mut pages_skipped = 0;

    cache
        .write_cache_pages(
            3,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            PAGE_SIZE_4K,
            &mut |_, data| {
                core::assert_eq!(
                    cache
                        .state
                        .lock()
                        .writeback_marks
                        .iter()
                        .copied()
                        .collect::<Vec<_>>(),
                    vec![0]
                );
                let waiting_cache = cache.clone();
                let waiting_entered = entered.clone();
                let waiting_finished = finished.clone();
                let waiting_ready = ready.clone();
                waiter = Some(ktask::spawn(move || {
                    waiting_entered.store(true, Ordering::Release);
                    waiting_ready.notify_all(false);
                    waiting_cache.wait_on_writeback_range(0..PAGE_SIZE_4K as u64);
                    waiting_finished.store(true, Ordering::Release);
                }));
                ready.wait_until(|| entered.load(Ordering::Acquire));
                core::assert!(!finished.load(Ordering::Acquire));
                Ok(data.len())
            },
        )
        .unwrap();

    assert_eq!(waiter.expect("writeback waiter").join(), 0);
    assert!(finished.load(Ordering::Acquire));
    assert!(cache.state.lock().writeback_marks.is_empty());
}

#[def_test]
fn batched_writeback_failure_cleans_only_complete_prefix_folios() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, PAGE_SIZE_4K as u64, &vec![2; PAGE_SIZE_4K]).unwrap();
    write_cached(&cache, (PAGE_SIZE_4K * 2) as u64, &vec![3; PAGE_SIZE_4K]).unwrap();

    let completed_bytes = PAGE_SIZE_4K + PAGE_SIZE_4K / 2;
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        (PAGE_SIZE_4K * 3) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        PAGE_SIZE_4K * 3,
        &mut |_, _| Err((completed_bytes, KError::InvalidInput)),
    );

    assert_eq!(outcome.err(), Some(KError::InvalidInput));
    assert_eq!(initial_budget - nr_to_write, 1);
    assert_eq!(dirty_indices(&cache), vec![1, 2]);
    for (index, expected_dirty) in [(0, false), (1, true), (2, true)] {
        let state = cache.with_folio(index, |folio| {
            let folio = folio.expect("batched folio remains cached");
            let state = (folio.is_dirty(), folio.is_under_writeback());
            folio.clear_dirty();
            state
        });
        assert_eq!(state, (expected_dirty, false));
    }
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn batched_writeback_preserves_concurrent_redirty() {
    let cache = new_cache();
    write_cached(&cache, 0, b"old").unwrap();

    let cache_during_io = &cache;
    let mut first_snapshot = Vec::new();
    let mut dirty_indices_before_redirty = Vec::new();
    let mut state_during_io = None;
    let mut dirty_indices_after_redirty = Vec::new();
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .write_cache_pages(
            3,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            PAGE_SIZE_4K,
            &mut |_, data| {
                dirty_indices_before_redirty = dirty_indices(cache_during_io);
                state_during_io = Some(cache_during_io.with_folio(0, |folio| {
                    let folio = folio.expect("folio under writeback");
                    (folio.is_dirty(), folio.is_under_writeback())
                }));
                first_snapshot.extend_from_slice(&data[..3]);
                write_cached(cache_during_io, 0, b"new").map_err(|error| (0, error))?;
                dirty_indices_after_redirty = dirty_indices(cache_during_io);
                Ok(data.len())
            },
        )
        .unwrap();

    assert!(dirty_indices_before_redirty.is_empty());
    assert_eq!(state_during_io, Some((false, true)));
    assert_eq!(dirty_indices_after_redirty, vec![0]);
    assert_eq!(first_snapshot, b"old");
    assert_eq!(dirty_indices(&cache), vec![0]);
    let observed = cache.with_folio(0, |folio| {
        let folio = folio.expect("redirtied folio");
        (
            folio.is_dirty(),
            folio.data()[..3].to_vec(),
            folio.is_under_writeback(),
        )
    });
    assert!(observed.0);
    assert_eq!(observed.1.as_slice(), b"new");
    assert!(!observed.2);

    cache
        .write_cache_pages(
            3,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            PAGE_SIZE_4K,
            &mut |_, data| Ok(data.len()),
        )
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
    let final_state = cache.with_folio(0, |folio| {
        let folio = folio.expect("clean folio");
        (folio.is_dirty(), folio.is_under_writeback())
    });
    assert_eq!(final_state, (false, false));
}

#[def_test]
fn writeback_scans_sparse_windows_without_pinning_the_whole_range() {
    let cache = new_cache();
    let count = crate::FolioBatch::CAPACITY * 2 + 3;
    for index in 0..count {
        write_cached(&cache, (index * 2 * PAGE_SIZE_4K) as u64, b"x").unwrap();
    }
    let last_index = ((count - 1) * 2) as u64;
    let last = cache.state.lock().entries[&last_index].clone();
    let mut distant_ref_count = 0;
    let mut writes = Vec::new();
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    cache
        .writeback_until(
            (count * 2 * PAGE_SIZE_4K) as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            &mut |index, _, _| {
                if index == 0 {
                    // Only the mapping and this test retain the distant page.
                    distant_ref_count = Arc::strong_count(&last);
                }
                writes.push(index);
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(distant_ref_count, 2);
    assert_eq!(initial_budget - nr_to_write, count);
    assert_eq!(
        writes,
        (0..count).map(|i| (i * 2) as u64).collect::<Vec<_>>()
    );
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn batched_writeback_bounds_submissions_and_completes_eof_tail() {
    let cache = new_cache();
    let capacity = crate::FolioBatch::CAPACITY;
    let visible_len = capacity * 2 * PAGE_SIZE_4K + 123;
    write_cached(&cache, 0, &vec![0x37; visible_len]).unwrap();
    let mut writes = Vec::new();
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        visible_len as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        usize::MAX,
        &mut |offset, data| {
            writes.push((offset, data.len()));
            assert!(data.iter().all(|byte| *byte == 0x37));
            Ok(data.len())
        },
    );
    assert_eq!(outcome.err(), None);
    assert_eq!(initial_budget - nr_to_write, capacity * 2 + 1);
    assert_eq!(
        writes,
        vec![
            (0, capacity * PAGE_SIZE_4K),
            ((capacity * PAGE_SIZE_4K) as u64, capacity * PAGE_SIZE_4K),
            ((capacity * 2 * PAGE_SIZE_4K) as u64, 123),
        ]
    );
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn batched_writeback_budget_stops_inside_second_window() {
    let cache = new_cache();
    let capacity = crate::FolioBatch::CAPACITY;
    let count = capacity * 2 + 1;
    write_cached(&cache, 0, &vec![1; count * PAGE_SIZE_4K]).unwrap();
    let mut lengths = Vec::new();
    let initial_budget = capacity + 2;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        (count * PAGE_SIZE_4K) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        usize::MAX,
        &mut |_, data| {
            lengths.push(data.len());
            Ok(data.len())
        },
    );
    assert_eq!(outcome, Ok(()));
    assert_eq!(initial_budget - nr_to_write, capacity + 2);
    assert_eq!(lengths, vec![capacity * PAGE_SIZE_4K, 2 * PAGE_SIZE_4K]);
    assert_eq!(
        dirty_indices(&cache),
        ((capacity + 2) as u64..count as u64).collect::<Vec<_>>()
    );
    cache.truncate_final().unwrap();
}

#[def_test]
fn second_batch_error_finishes_prefix_and_leaves_unvisited_pages_dirty() {
    let cache = new_cache();
    let capacity = crate::FolioBatch::CAPACITY;
    let count = capacity * 2 + 3;
    write_cached(&cache, 0, &vec![1; count * PAGE_SIZE_4K]).unwrap();
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        (count * PAGE_SIZE_4K) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        usize::MAX,
        &mut |offset, data| {
            if offset == 0 {
                Ok(data.len())
            } else {
                Err((PAGE_SIZE_4K + 123, KError::InvalidInput))
            }
        },
    );
    assert_eq!(outcome.err(), Some(KError::InvalidInput));
    assert_eq!(initial_budget - nr_to_write, capacity + 1);
    assert_eq!(
        dirty_indices(&cache),
        ((capacity + 1) as u64..count as u64).collect::<Vec<_>>()
    );
    for index in 0..count as u64 {
        cache.with_folio(index, |folio| assert!(!folio.unwrap().is_under_writeback()));
    }
    cache.truncate_final().unwrap();
}

#[def_test]
fn redirty_in_previous_window_is_left_for_the_next_pass() {
    let cache = new_cache();
    let count = crate::FolioBatch::CAPACITY * 2 + 1;
    write_cached(&cache, 0, &vec![1; count * PAGE_SIZE_4K]).unwrap();
    let initial_budget = usize::MAX;
    let mut nr_to_write = initial_budget;
    let mut pages_skipped = 0;
    let outcome = cache.write_cache_pages(
        (count * PAGE_SIZE_4K) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        usize::MAX,
        &mut |_, data| {
            write_cached(&cache, 0, b"new").map_err(|error| (0, error))?;
            Ok(data.len())
        },
    );
    assert_eq!(outcome, Ok(()));
    assert_eq!(initial_budget - nr_to_write, count);
    assert_eq!(dirty_indices(&cache), vec![0]);
    cache.truncate_final().unwrap();
}

#[def_test]
fn skipped_and_completed_pages_remain_accounted_after_error() {
    let cache = new_cache();
    write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K * 3]).unwrap();
    // Model a folio redirtied while another writeback owns it.
    cache.with_folio(0, |folio| folio.unwrap().start_writeback());
    let mut nr_to_write = 3;
    let mut pages_skipped = 7;
    let result = cache.writeback_until(
        (PAGE_SIZE_4K * 3) as u64,
        0..u64::MAX,
        &mut nr_to_write,
        &mut pages_skipped,
        &mut |index, _, _| {
            if index == 2 {
                Err(KError::InvalidInput)
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(result, Err(KError::InvalidInput));
    assert_eq!(nr_to_write, 2);
    assert_eq!(pages_skipped, 8);
    assert_eq!(dirty_indices(&cache), vec![0, 2]);
    cache.with_folio(0, |folio| folio.unwrap().end_writeback());
    cache.truncate_final().unwrap();
}

#[def_test]
fn batch_prefix_validation_preserves_progress_and_finishes_all_folios() {
    for (prefix, expected_remaining) in [(PAGE_SIZE_4K, 1), (PAGE_SIZE_4K * 2 + 1, 2)] {
        let cache = new_cache();
        write_cached(&cache, 0, &vec![1; PAGE_SIZE_4K * 2]).unwrap();
        let mut nr_to_write = 2;
        let mut pages_skipped = 0;
        let result = cache.write_cache_pages(
            (PAGE_SIZE_4K * 2) as u64,
            0..u64::MAX,
            &mut nr_to_write,
            &mut pages_skipped,
            PAGE_SIZE_4K * 2,
            &mut |_, _| Ok(prefix),
        );
        let expected_result = if prefix <= PAGE_SIZE_4K * 2 {
            Ok(())
        } else {
            Err(KError::InvalidInput)
        };
        assert_eq!(result, expected_result);
        assert_eq!(nr_to_write, expected_remaining);
        for index in 0..2 {
            cache.with_folio(index, |folio| assert!(!folio.unwrap().is_under_writeback()));
        }
        cache.truncate_final().unwrap();
    }
}
