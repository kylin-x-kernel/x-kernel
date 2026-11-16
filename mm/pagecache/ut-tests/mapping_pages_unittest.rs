// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resident storage tests.

use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use super::{dirty_indices, new_cache, write_cached};

#[def_test]
fn cached_folio_roundtrip() {
    let cache = new_cache();
    write_cached(&cache, 0, b"hello").unwrap();

    let observed = cache
        .with_folio(0, |folio| {
            let folio = folio.expect("cached folio");
            let observed = folio.data()[..5].to_vec();
            folio.clear_dirty();
            observed
        })
        .unwrap();
    assert_eq!(observed.as_slice(), b"hello");
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn cache_storage_does_not_own_visible_length() {
    let cache = new_cache();
    write_cached(&cache, PAGE_SIZE_4K as u64, b"x").unwrap();

    assert_eq!(cache.nrpages(), 1);
    cache
        .resize_cached_folios((PAGE_SIZE_4K * 2) as u64, 0)
        .unwrap();
    assert_eq!(cache.nrpages(), 0);
}

#[def_test]
fn failed_materialization_is_not_visible_and_can_be_retried() {
    let cache = new_cache();
    let result = cache.with_folio_or_create(
        7,
        |mut folio| {
            folio.data().fill(0xff);
            Err(kerrno::KError::Io)
        },
        |_| panic!("incomplete data must not be exposed"),
    );
    assert_eq!(result, Err::<(), _>(kerrno::KError::Io));
    assert_eq!(cache.nrpages(), 1);
    cache
        .with_folio(7, |folio| assert!(folio.is_none()))
        .unwrap();
    cache
        .resize_cached_folios(8 * PAGE_SIZE_4K as u64, 7 * PAGE_SIZE_4K as u64 + 17)
        .unwrap();
    cache
        .resize_cached_folios(7 * PAGE_SIZE_4K as u64 + 17, 8 * PAGE_SIZE_4K as u64)
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
    cache
        .with_folio_or_create(
            7,
            |mut folio| {
                folio.data().fill(0x5a);
                folio.complete(Ok(()))
            },
            |folio| {
                assert!(folio.is_uptodate());
                assert!(folio.data().iter().all(|&byte| byte == 0x5a));
                Ok(())
            },
        )
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn folio_index_survives_detach_and_replacement() {
    let cache = new_cache();
    let index = 7;
    write_cached(&cache, index * PAGE_SIZE_4K as u64, b"old").unwrap();
    let old = cache.state.lock().entries[&index].clone();
    cache
        .resize_cached_folios((index + 1) * PAGE_SIZE_4K as u64, 0)
        .unwrap();
    cache
        .filemap_add_folio(super::folio_from_bytes(index, b"new"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    let replacement = cache.state.lock().entries[&index].clone();
    assert!(!alloc::sync::Arc::ptr_eq(&old, &replacement));
    assert_eq!(old.lock().unwrap().index(), index);
    assert!(!old.lock().unwrap().is_dirty());
    assert_eq!(replacement.lock().unwrap().index(), index);
    cache
        .with_folio(index, |folio| {
            assert!(folio.unwrap().data()[..3] == *b"new")
        })
        .unwrap();
}

#[def_test]
fn publication_preserves_resident_data_and_missing_run_boundaries() {
    let cache = new_cache();
    let mut incoming = super::folio_from_bytes(1, b"stale");
    incoming.mark_dirty();
    let incoming_addr = incoming.paddr();
    write_cached(&cache, PAGE_SIZE_4K as u64, b"winner").unwrap();
    let mut rejected = cache.filemap_add_folio(incoming).unwrap_err();
    assert_eq!(rejected.paddr(), incoming_addr);
    assert!(rejected.is_dirty());
    core::assert_eq!(&rejected.data()[..5], b"stale");
    rejected.lock().unwrap().clear_dirty();
    cache
        .with_folio(1, |folio| {
            let folio = folio.unwrap();
            core::assert_eq!(&folio.data()[..6], b"winner");
            assert!(folio.is_dirty());
        })
        .unwrap();
    assert_eq!(cache.next_miss_range(0..4), Some(0..1));
    assert_eq!(cache.next_miss_range(1..4), Some(2..4));
    assert_eq!(cache.next_miss_range(1..2), None);
    cache
        .resize_cached_folios((PAGE_SIZE_4K * 4) as u64, 0)
        .unwrap();
}

#[def_test]
fn concurrent_materialization_waits_and_retries_failed_reads() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ktask::WaitQueue;

    for fail_first in [false, true] {
        let cache = Arc::new(new_cache());
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(WaitQueue::new());
        let first_cache = cache.clone();
        let first_entered = entered.clone();
        let first_release = release.clone();
        let first_reads = reads.clone();
        let first_events = events.clone();
        let first = ktask::spawn(move || {
            let result = first_cache.with_folio_or_create(
                0,
                |mut folio| {
                    first_reads.fetch_add(1, Ordering::Relaxed);
                    folio.data().fill(0xff);
                    first_entered.store(true, Ordering::Release);
                    first_events.notify_all(false);
                    first_events.wait_until(|| first_release.load(Ordering::Acquire));
                    if fail_first {
                        return Err(kerrno::KError::Io);
                    }
                    folio.data().fill(0x5a);
                    folio.complete(Ok(()))
                },
                |folio| {
                    assert!(folio.data().iter().all(|&byte| byte == 0x5a));
                    Ok(())
                },
            );
            core::assert_eq!(
                result,
                if fail_first {
                    Err(kerrno::KError::Io)
                } else {
                    Ok(())
                }
            );
        });
        events.wait_until(|| entered.load(Ordering::Acquire));
        // Accessing the index while I/O is blocked also proves that I/O does
        // not retain the global index mutex.
        let resident = cache.state.lock().entries[&0].clone();
        *super::READ_WAIT_SIGNAL.lock() = Some((resident.clone(), waiting.clone(), events.clone()));
        let second_cache = cache.clone();
        let second_reads = reads.clone();
        let second = ktask::spawn(move || {
            second_cache
                .with_folio_or_create(
                    0,
                    |mut folio| {
                        second_reads.fetch_add(1, Ordering::Relaxed);
                        folio.data().fill(0x5a);
                        folio.complete(Ok(()))
                    },
                    |folio| {
                        assert!(folio.is_uptodate());
                        assert!(folio.data().iter().all(|&byte| byte == 0x5a));
                        Ok(())
                    },
                )
                .unwrap();
        });
        events.wait_until(|| waiting.load(Ordering::Acquire));
        let reads_before_release = reads.load(Ordering::Relaxed);
        release.store(true, Ordering::Release);
        events.notify_all(false);
        assert_eq!(first.join(), 0);
        assert_eq!(second.join(), 0);
        super::READ_WAIT_SIGNAL.lock().take();
        assert_eq!(reads_before_release, 1);
        assert_eq!(
            reads.load(Ordering::Relaxed),
            if fail_first { 2 } else { 1 }
        );
        assert!(Arc::ptr_eq(&cache.state.lock().entries[&0], &resident));
    }

    // Use the same observer sequentially: the waiter must see the old locked
    // reservation, then retry after readahead removes it before unlocking.
    let cache = Arc::new(new_cache());
    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let waiting = Arc::new(AtomicBool::new(false));
    let events = Arc::new(WaitQueue::new());
    let producer_cache = cache.clone();
    let producer_entered = entered.clone();
    let producer_release = release.clone();
    let producer_events = events.clone();
    let producer = ktask::spawn(move || {
        let prepared = producer_cache.readahead_folios(0..2, |folios| {
            let mut first = folios.next().unwrap();
            first.data().fill(0x11);
            first.complete(Ok(())).unwrap();
            producer_entered.store(true, Ordering::Release);
            producer_events.notify_all(false);
            producer_events.wait_until(|| producer_release.load(Ordering::Acquire));
        });
        core::assert_eq!(prepared, Ok(2));
    });
    events.wait_until(|| entered.load(Ordering::Acquire));
    let reserved = cache.state.lock().entries[&1].clone();
    *super::READ_WAIT_SIGNAL.lock() = Some((reserved.clone(), waiting.clone(), events.clone()));
    let reader_cache = cache.clone();
    let reader = ktask::spawn(move || {
        reader_cache
            .with_folio_or_create(
                1,
                |mut folio| {
                    folio.data().fill(0x5a);
                    folio.complete(Ok(()))
                },
                |folio| {
                    folio.mark_dirty();
                    Ok(())
                },
            )
            .unwrap();
    });
    events.wait_until(|| waiting.load(Ordering::Acquire));
    // A blocked reader must not hold the index lock.
    let pages_before_release = cache.nrpages();
    release.store(true, Ordering::Release);
    events.notify_all(false);
    assert_eq!(producer.join(), 0);
    assert_eq!(reader.join(), 0);
    super::READ_WAIT_SIGNAL.lock().take();
    assert_eq!(pages_before_release, 2);
    assert_eq!(cache.nrpages(), 2);
    assert!(!Arc::ptr_eq(&cache.state.lock().entries[&1], &reserved));
    assert!(!reserved.lock().unwrap().is_uptodate());
    cache
        .with_folio(1, |folio| {
            let folio = folio.unwrap();
            core::assert!(folio.is_dirty());
            core::assert!(folio.data().iter().all(|byte| *byte == 0x5a));
        })
        .unwrap();
    assert_eq!(dirty_indices(&cache), alloc::vec![1]);
}

#[def_test]
fn readahead_releases_unconsumed_reservations_and_retains_attempted_reads() {
    let cache = new_cache();
    assert_eq!(cache.readahead_folios(0..4, |_| {}), Ok(4));
    assert_eq!(cache.nrpages(), 0);
    assert_eq!(cache.next_miss_range(0..4), Some(0..4));

    assert_eq!(
        cache.readahead_folios(0..4, |folios| {
            let mut complete = folios.next().unwrap();
            complete.data().fill(0x11);
            complete.complete(Ok(())).unwrap();
            let mut failed = folios.next().unwrap();
            failed.data()[0] = 0xff;
            // Returning here models a failed second read and an untouched suffix.
        }),
        Ok(4)
    );
    assert_eq!(cache.nrpages(), 2);
    cache
        .with_folio(0, |folio| core::assert_eq!(folio.unwrap().data()[0], 0x11))
        .unwrap();
    cache
        .with_folio(1, |folio| assert!(folio.is_none()))
        .unwrap();
    assert_eq!(cache.next_miss_range(0..4), Some(2..4));
    cache
        .with_folio_or_create(
            1,
            |mut folio| {
                folio.data().fill(0x22);
                folio.complete(Ok(()))
            },
            |folio| {
                core::assert_eq!(folio.data()[0], 0x22);
                Ok(())
            },
        )
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test(serial)]
fn transferred_read_completes_without_task_or_mapping_lock_ownership() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ktask::WaitQueue;

    // Successful completion, explicit I/O failure, and abandoned submission.
    for outcome in 0..3 {
        let cache = Arc::new(new_cache());
        let mut pending = None;
        assert_eq!(
            cache.readahead_folios(0..2, |folios| pending = folios.next()),
            Ok(2)
        );
        assert_eq!(
            cache.nrpages(),
            1,
            "unconsumed suffix must already be removed"
        );
        let mut pending = pending.unwrap();
        pending.data().fill(0xff);
        let resident = cache.state.lock().entries[&0].clone();
        let waiting = Arc::new(AtomicBool::new(false));
        let events = Arc::new(WaitQueue::new());
        let reads = Arc::new(AtomicUsize::new(0));
        *super::READ_WAIT_SIGNAL.lock() = Some((resident.clone(), waiting.clone(), events.clone()));

        let reader_cache = cache.clone();
        let reader_reads = reads.clone();
        let reader = ktask::spawn(move || {
            reader_cache
                .with_folio_or_create(
                    0,
                    |mut folio| {
                        reader_reads.fetch_add(1, Ordering::Relaxed);
                        folio.data().fill(0x5a);
                        folio.complete(Ok(()))
                    },
                    |folio| {
                        assert!(folio.is_uptodate());
                        assert!(folio.data().iter().all(|&byte| byte == 0x5a));
                        Ok(())
                    },
                )
                .unwrap();
        });
        events.wait_until(|| waiting.load(Ordering::Acquire));
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert!(resident.try_lock().is_none());

        // The receiver enters hardirq context while the sender retains the
        // index lock and a reader is enrolled. Completion may need neither
        // sleepable context nor the mapping lock to publish and wake it.
        let state = cache.state.lock();
        let completion = ktask::spawn(move || {
            let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
            match outcome {
                0 => {
                    pending.data().fill(0x5a);
                    pending.complete(Ok(())).unwrap();
                }
                1 => core::assert_eq!(
                    pending.complete(Err(kerrno::KError::Io)),
                    Err(kerrno::KError::Io)
                ),
                _ => drop(pending),
            }
        });
        assert_eq!(completion.join(), 0);
        drop(state);
        assert_eq!(reader.join(), 0);
        super::READ_WAIT_SIGNAL.lock().take();
        assert_eq!(reads.load(Ordering::Relaxed), usize::from(outcome != 0));
        assert!(Arc::ptr_eq(&resident, &cache.state.lock().entries[&0]));
    }
}

#[def_test]
fn split_read_publishes_only_after_all_parts_retire() {
    use alloc::sync::Arc;
    use kerrno::KError;

    for outcome in 0..3 {
        let folio = Arc::new(crate::Folio::new_zeroed(7).unwrap());
        let read = folio.lock().unwrap().into_read(&folio);
        let read = read.split_at(0).err().unwrap();
        let (mut first, tail) = read.split_at(1024).unwrap_or_else(|_| panic!("valid split"));
        let (mut middle, mut last) = tail.split_at(1024).unwrap_or_else(|_| panic!("valid split"));
        assert_eq!(first.offset(), 0);
        assert_eq!(middle.offset(), 1024);
        assert_eq!(last.offset(), 2048);
        let first_address = first.data().as_ptr() as usize;
        assert_eq!(middle.data().as_ptr() as usize, first_address + 1024);
        assert_eq!(last.data().as_ptr() as usize, first_address + 2048);
        first.data().fill(0x11);
        last.data().fill(0x33);
        last.complete(Ok(())).unwrap();
        assert!(folio.try_lock().is_none());
        let completion = ktask::spawn(move || {
            middle.data().fill(0x22);
            let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
            match outcome {
                0 => middle.complete(Ok(())).unwrap(),
                1 => core::assert_eq!(middle.complete(Err(KError::Io)), Err(KError::Io)),
                _ => drop(middle),
            }
        });
        assert_eq!(completion.join(), 0);
        assert!(folio.try_lock().is_none(), "first part still owns device access");
        first.complete(Ok(())).unwrap();
        let mut guard = folio.try_lock().expect("last part must release the lock");
        assert_eq!(guard.is_uptodate(), outcome == 0);
        if outcome == 0 {
            assert!(guard.data()[..1024].iter().all(|&byte| byte == 0x11));
            assert!(guard.data()[1024..2048].iter().all(|&byte| byte == 0x22));
            assert!(guard.data()[2048..].iter().all(|&byte| byte == 0x33));
        }
    }
}

#[def_test(serial)]
fn demand_read_waits_for_owned_completion_and_reports_failure() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use ksync::Mutex;
    use ktask::WaitQueue;

    for succeeds in [false, true] {
        let cache = Arc::new(new_cache());
        let pending = Arc::new(Mutex::new(None));
        let waiting = Arc::new(AtomicBool::new(false));
        let events = Arc::new(WaitQueue::new());
        let reader_cache = cache.clone();
        let reader_pending = pending.clone();
        let reader_waiting = waiting.clone();
        let reader_events = events.clone();
        let reader = ktask::spawn(move || {
            let result = reader_cache.with_folio_or_create(
                0,
                |folio| {
                    let resident = reader_cache.state.lock().entries[&0].clone();
                    *super::READ_WAIT_SIGNAL.lock() =
                        Some((resident, reader_waiting.clone(), reader_events.clone()));
                    *reader_pending.lock() = Some(folio);
                    // Submission succeeds while the controller still owns completion.
                    Ok(())
                },
                |folio| {
                    assert!(succeeds);
                    assert!(folio.data().iter().all(|&byte| byte == 0x5a));
                    Ok(())
                },
            );
            core::assert_eq!(
                result,
                if succeeds {
                    Ok(())
                } else {
                    Err(kerrno::KError::Io)
                }
            );
        });
        events.wait_until(|| waiting.load(Ordering::Acquire));
        let mut folio = pending.lock().take().unwrap();
        folio.data().fill(0x5a);
        let result = if succeeds {
            Ok(())
        } else {
            Err(kerrno::KError::Io)
        };
        assert_eq!(folio.complete(result), result);
        assert_eq!(reader.join(), 0);
        super::READ_WAIT_SIGNAL.lock().take();
        cache
            .with_folio(0, |folio| core::assert_eq!(folio.is_some(), succeeds))
            .unwrap();
    }
}

#[def_test(serial)]
fn explicit_truncate_waits_for_transferred_initialization() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktask::WaitQueue;

    // Partial EOF and full removal explicitly drain initialization.
    for action in 0..2 {
        let cache = Arc::new(new_cache());
        let mut pending = None;
        cache
            .readahead_folios(0..1, |folios| pending = folios.next())
            .unwrap();
        let mut pending = pending.unwrap();
        let resident = cache.state.lock().entries[&0].clone();
        let old = Arc::downgrade(&resident);
        let waiting = Arc::new(AtomicBool::new(false));
        let events = Arc::new(WaitQueue::new());
        *super::READ_WAIT_SIGNAL.lock() = Some((resident.clone(), waiting.clone(), events.clone()));
        let retained = cache.clone();
        let truncator = ktask::spawn(move || {
            cache
                .resize_cached_folios(PAGE_SIZE_4K as u64, if action == 0 { 17 } else { 0 })
                .unwrap();
            drop(cache);
        });
        events.wait_until(|| waiting.load(Ordering::Acquire));
        assert!(resident.try_lock().is_none());
        pending.data().fill(0x5a);
        pending.complete(Ok(())).unwrap();
        assert_eq!(truncator.join(), 0);
        super::READ_WAIT_SIGNAL.lock().take();
        {
            let cache = &retained;
            if action == 0 {
                cache
                    .with_folio(0, |folio| {
                        let folio = folio.unwrap();
                        assert!(folio.data()[..17].iter().all(|&byte| byte == 0x5a));
                        assert!(folio.data()[17..].iter().all(|&byte| byte == 0));
                    })
                    .unwrap();
            } else {
                assert_eq!(cache.nrpages(), 0);
                assert!(resident.lock().unwrap().is_invalidated());
                write_cached(cache, 0, b"replacement").unwrap();
                assert!(!Arc::ptr_eq(&resident, &cache.state.lock().entries[&0]));
            }
        }
        drop(retained);
        drop(resident);
        assert!(old.upgrade().is_none());
    }
}

#[def_test]
fn resident_batch_preserves_sparse_positions_and_bounds_the_snapshot() {
    use core::ops::ControlFlow;

    let cache = new_cache();
    for index in (0..crate::FolioBatch::CAPACITY + 5).step_by(2) {
        write_cached(&cache, (index * PAGE_SIZE_4K) as u64, &[index as u8]).unwrap();
    }
    let mut visited = 0;
    core::assert_eq!(
        cache.visit_resident_folios(
            0,
            crate::FolioBatch::CAPACITY + 5,
            |_| panic!("initialized resident hits must not reread storage"),
            |index, folio| {
                assert!(
                    cache.state.try_lock().is_some(),
                    "copy must run outside the index lock"
                );
                core::assert_eq!(index, visited);
                if index % 2 == 0 {
                    core::assert_eq!(folio.unwrap().data()[0], index as u8);
                } else {
                    assert!(folio.is_none());
                }
                visited += 1;
                Ok(ControlFlow::Continue(()))
            },
        ),
        Ok(crate::FolioBatch::CAPACITY)
    );
    core::assert_eq!(visited, crate::FolioBatch::CAPACITY as u64);
    core::assert_eq!(
        cache.nrpages(),
        ((crate::FolioBatch::CAPACITY + 5).div_ceil(2)) as u64
    );
}

#[def_test]
fn resident_batch_stops_before_initializing_the_next_page() {
    use core::ops::ControlFlow;

    let cache = new_cache();
    write_cached(&cache, 0, b"first").unwrap();
    core::assert_eq!(
        cache.with_folio_or_create(1, |_| Err(kerrno::KError::Io), |_| Ok(())),
        Err(kerrno::KError::Io)
    );
    core::assert_eq!(
        cache.visit_resident_folios(
            0,
            2,
            |_| panic!("a short read must not initialize the following reservation"),
            |index, folio| {
                core::assert_eq!(index, 0);
                core::assert_eq!(&folio.unwrap().data()[..5], b"first");
                Ok(ControlFlow::Break(()))
            }
        ),
        Ok(1)
    );
}

#[def_test]
fn index_check_accepts_reconciled_dirtying() {
    let cache = new_cache();
    write_cached(&cache, 0, b"hello").unwrap();
    cache.verify_index_consistency();
}

#[def_test]
fn index_check_detects_each_mismatch_kind() {
    use super::index_check::IndexMismatch;

    let cache = new_cache();
    write_cached(&cache, 0, b"hello").unwrap();
    cache.state.lock().dirty_marks.remove(&0);
    let mismatches = cache.index_mismatches();
    assert!(
        mismatches.contains(&IndexMismatch::Dirty {
            index: 0,
            bit: true,
            mark: false,
        }),
        "a missing dirty mark must be reported: {mismatches:?}"
    );

    let cache = new_cache();
    write_cached(&cache, 0, b"hello").unwrap();
    cache.state.lock().writeback_marks.insert(0);
    let mismatches = cache.index_mismatches();
    assert!(
        mismatches.contains(&IndexMismatch::Writeback {
            index: 0,
            bit: false,
            mark: true,
        }),
        "a stale writeback mark must be reported: {mismatches:?}"
    );

    let cache = new_cache();
    cache.state.lock().dirty_marks.insert(9);
    let mismatches = cache.index_mismatches();
    assert!(
        mismatches.contains(&IndexMismatch::Orphan {
            index: 9,
            dirty: true,
            writeback: false,
        }),
        "an orphaned dirty mark must be reported: {mismatches:?}"
    );
}
