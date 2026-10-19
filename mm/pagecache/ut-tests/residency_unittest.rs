// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page-quota accounting across resident reservation lifecycles.

use alloc::sync::Arc;

use kerrno::KError;
use unittest::{assert_eq, def_test};

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::{CacheResidency, Folio, MappingPages};

struct TestResidency {
    limit: usize,
    used: AtomicUsize,
}

impl TestResidency {
    fn new(limit: usize) -> Self {
        Self { limit, used: AtomicUsize::new(0) }
    }

    fn used_pages(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

impl CacheResidency for TestResidency {
    fn try_add_folio(&self) -> kerrno::KResult<()> {
        self.used.fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            (used < self.limit).then_some(used + 1)
        }).map(|_| ()).map_err(|_| KError::StorageFull)
    }

    fn remove_folios(&self, count: usize) {
        assert!(self.used.fetch_sub(count, Ordering::AcqRel) >= count);
    }
}

#[def_test]
fn page_quota_limits_resident_entries_and_unpublished_pages_are_unaccounted() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let unpublished = Folio::new_zeroed(0).unwrap();
    assert_eq!(quota.used_pages(), 0);
    drop(unpublished);
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    assert_eq!(
        cache.with_folio_or_create(1, |_| Ok(()), |_| Ok(())),
        Err(KError::StorageFull)
    );
    assert_eq!(cache.nrpages(), 1);
    drop(cache);
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn page_quota_releases_when_folio_leaves_the_mapping() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    let external = cache.state.lock().entries[&0].clone();
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    drop(external);
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn truncate_releases_quota_and_invalidates_retained_read_batches() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(
            0,
            |folio| {
                folio.data().fill(0x55);
                Ok(())
            },
            |_| Ok(()),
        )
        .unwrap();
    let batch = cache.get_folios_contig(0, 1);
    assert_eq!(batch.with_read_folio(0, |folio| folio.data()[0]), Some(0x55));
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    assert_eq!(batch.with_read_folio(0, |folio| folio.data()[0]), None);
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    drop(batch);
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn resident_lookup_does_not_charge_again_when_quota_is_full() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    cache
        .with_folio_or_create(
            0,
            |_| panic!("resident is initialized"),
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(
        cache.with_folio_or_create(1, |_| Ok(()), |_| Ok(())),
        Err(KError::StorageFull)
    );
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn readahead_reservations_charge_and_release_unconsumed_pages() {
    let quota = Arc::new(TestResidency::new(2));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    assert_eq!(
        cache
            .readahead_folios(0..4, |folios| {
                folios.next().unwrap().mark_uptodate();
            })
            .unwrap(),
        2
    );
    assert_eq!(quota.used_pages(), 1);
    assert_eq!(cache.nrpages(), 1);
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn same_index_allocators_share_the_last_quota_slot() {
    use core::sync::atomic::Ordering;

    use ksync::Mutex;

    for slow_is_readahead in [false, true] {
        for winner_is_readahead in [false, true] {
            let quota = Arc::new(TestResidency::new(1));
            let cache = Arc::new(MappingPages::with_residency(Some(quota.clone())));
            let gate = Arc::new(super::AllocationGate::new());
            *cache.allocation_gate.lock() = Some(gate.clone());
            let result = Arc::new(Mutex::new(None));
            let slow_cache = cache.clone();
            let slow_result = result.clone();
            let slow = ktask::spawn(move || {
                let outcome = if slow_is_readahead {
                    slow_cache.readahead_folios(0..1, |_| {
                        panic!("the winner already owns this cache identity");
                    })
                } else {
                    slow_cache.with_folio_or_create(
                        0,
                        |_| panic!("the winner initialized the resident"),
                        |folio| {
                            core::assert_eq!(folio.data()[0], 0x5a);
                            Ok(0)
                        },
                    )
                };
                *slow_result.lock() = Some(outcome);
            });
            gate.events
                .wait_until(|| gate.entered.load(Ordering::Acquire));
            let winner = if winner_is_readahead {
                cache
                    .readahead_folios(0..1, |folios| {
                        let folio = folios.next().unwrap();
                        folio.data().fill(0x5a);
                        folio.mark_uptodate();
                    })
                    .map(|_| ())
            } else {
                cache.with_folio_or_create(
                    0,
                    |folio| {
                        folio.data().fill(0x5a);
                        Ok(())
                    },
                    |_| Ok(()),
                )
            };
            gate.released.store(true, Ordering::Release);
            gate.events.notify_all(false);
            assert_eq!(slow.join(), 0);
            cache.allocation_gate.lock().take();
            assert_eq!(winner, Ok(()));
            assert_eq!(result.lock().take(), Some(Ok(0)));
            assert_eq!(cache.nrpages(), 1);
            assert_eq!(quota.used_pages(), 1);
        }
    }
}

#[def_test]
fn mapping_drop_returns_quota_before_retained_batch_is_dropped() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    let batch = cache.get_folios_contig(0, 1);
    drop(cache);
    assert_eq!(batch.with_read_folio(0, |_| ()), None);
    assert_eq!(quota.used_pages(), 0);
    let replacement = MappingPages::with_residency(Some(quota.clone()));
    replacement
        .with_folio_or_create(0, |_| Ok(()), |_| Ok(()))
        .unwrap();
    drop(batch);
    assert_eq!(quota.used_pages(), 1);
}
