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

    let observed = cache.with_folio(0, |folio| {
        let folio = folio.expect("cached folio");
        let observed = folio.data()[..5].to_vec();
        folio.clear_dirty();
        observed
    });
    assert_eq!(observed.as_slice(), b"hello");
    assert!(dirty_indices(&cache).is_empty());
}

#[def_test]
fn cache_storage_does_not_own_visible_length() {
    let cache = new_cache();
    write_cached(&cache, PAGE_SIZE_4K as u64, b"x").unwrap();

    assert_eq!(cache.nrpages(), 1);
    cache.resize_cached_folios((PAGE_SIZE_4K * 2) as u64, 0);
    assert_eq!(cache.nrpages(), 0);
}

#[def_test]
fn failed_materialization_is_not_visible_and_can_be_retried() {
    let cache = new_cache();
    let result = cache.with_folio_or_create(
        7,
        |folio| {
            folio.data().fill(0xff);
            Err(kerrno::KError::Io)
        },
        |_| panic!("incomplete data must not be exposed"),
    );
    assert_eq!(result, Err::<(), _>(kerrno::KError::Io));
    assert_eq!(cache.nrpages(), 1);
    cache.with_folio(7, |folio| assert!(folio.is_none()));
    cache.resize_cached_folios(8 * PAGE_SIZE_4K as u64, 7 * PAGE_SIZE_4K as u64 + 17);
    cache.resize_cached_folios(7 * PAGE_SIZE_4K as u64 + 17, 8 * PAGE_SIZE_4K as u64);
    assert!(dirty_indices(&cache).is_empty());
    cache
        .with_folio_or_create(
            7,
            |folio| {
                folio.data().fill(0x5a);
                Ok(())
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
    cache.resize_cached_folios((index + 1) * PAGE_SIZE_4K as u64, 0);
    cache
        .filemap_add_folio(super::folio_from_bytes(index, b"new"))
        .unwrap_or_else(|_| panic!("fixture index is absent"));
    let replacement = cache.state.lock().entries[&index].clone();
    assert!(!alloc::sync::Arc::ptr_eq(&old, &replacement));
    assert_eq!(old.lock().index(), index);
    assert!(!old.lock().is_dirty());
    assert_eq!(replacement.lock().index(), index);
    cache.with_folio(index, |folio| {
        assert!(folio.unwrap().data()[..3] == *b"new")
    });
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
    rejected.lock().clear_dirty();
    cache.with_folio(1, |folio| {
        let folio = folio.unwrap();
        core::assert_eq!(&folio.data()[..6], b"winner");
        assert!(folio.is_dirty());
    });
    assert_eq!(cache.next_miss_range(0..4), Some(0..1));
    assert_eq!(cache.next_miss_range(1..4), Some(2..4));
    assert_eq!(cache.next_miss_range(1..2), None);
    cache.resize_cached_folios((PAGE_SIZE_4K * 4) as u64, 0);
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
                |folio| {
                    first_reads.fetch_add(1, Ordering::Relaxed);
                    folio.data().fill(0xff);
                    first_entered.store(true, Ordering::Release);
                    first_events.notify_all(false);
                    first_events.wait_until(|| first_release.load(Ordering::Acquire));
                    if fail_first {
                        return Err(kerrno::KError::Io);
                    }
                    folio.data().fill(0x5a);
                    Ok(())
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
                    |folio| {
                        second_reads.fetch_add(1, Ordering::Relaxed);
                        folio.data().fill(0x5a);
                        Ok(())
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
        assert!(Arc::ptr_eq(
            &cache.state.lock().entries[&0],
            &resident
        ));
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
            let first = folios.next().unwrap();
            first.data().fill(0x11);
            first.mark_uptodate();
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
                |folio| {
                    folio.data().fill(0x5a);
                    Ok(())
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
    assert!(!Arc::ptr_eq(
        &cache.state.lock().entries[&1],
        &reserved
    ));
    assert!(!reserved.lock().is_uptodate());
    cache.with_folio(1, |folio| {
        let folio = folio.unwrap();
        core::assert!(folio.is_dirty());
        core::assert!(folio.data().iter().all(|byte| *byte == 0x5a));
    });
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
            let complete = folios.next().unwrap();
            complete.data().fill(0x11);
            complete.mark_uptodate();
            let failed = folios.next().unwrap();
            failed.data()[0] = 0xff;
            // Returning here models a failed second read and an untouched suffix.
        }),
        Ok(4)
    );
    assert_eq!(cache.nrpages(), 2);
    cache.with_folio(0, |folio| core::assert_eq!(folio.unwrap().data()[0], 0x11));
    cache.with_folio(1, |folio| assert!(folio.is_none()));
    assert_eq!(cache.next_miss_range(0..4), Some(2..4));
    cache
        .with_folio_or_create(
            1,
            |folio| {
                folio.data().fill(0x22);
                Ok(())
            },
            |folio| {
                core::assert_eq!(folio.data()[0], 0x22);
                Ok(())
            },
        )
        .unwrap();
    assert!(dirty_indices(&cache).is_empty());
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
                assert!(cache.state.try_lock().is_some(), "copy must run outside the index lock");
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
    core::assert_eq!(cache.nrpages(), ((crate::FolioBatch::CAPACITY + 5).div_ceil(2)) as u64);
}

#[def_test]
fn resident_batch_stops_before_initializing_the_next_page() {
    use core::ops::ControlFlow;

    let cache = new_cache();
    write_cached(&cache, 0, b"first").unwrap();
    core::assert_eq!(cache.with_folio_or_create(1, |_| Err(kerrno::KError::Io), |_| Ok(())), Err(kerrno::KError::Io));
    core::assert_eq!(
        cache.visit_resident_folios(0, 2,
            |_| panic!("a short read must not initialize the following reservation"),
            |index, folio| {
                core::assert_eq!(index, 0);
                core::assert_eq!(&folio.unwrap().data()[..5], b"first");
                Ok(ControlFlow::Break(()))
            }),
        Ok(1)
    );
}
