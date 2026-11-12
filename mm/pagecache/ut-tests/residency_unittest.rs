// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache-residency quota accounting across reservation, publication, and detach.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kerrno::KError;
use memaddr::PAGE_SIZE_4K;
use unittest::{assert_eq, def_test};

use crate::{CacheResidency, Folio, MappingPages};

struct TestResidency {
    limit: usize,
    used: AtomicUsize,
}

impl TestResidency {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    fn used_pages(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

impl CacheResidency for TestResidency {
    fn try_add_folio(&self) -> kerrno::KResult<()> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.limit).then_some(used + 1)
            })
            .map(|_| ())
            .map_err(|_| KError::StorageFull)
    }

    fn remove_folio(&self) {
        assert!(self.used.fetch_sub(1, Ordering::AcqRel) >= 1);
    }
}

#[def_test]
fn detached_pins_keep_physical_pages_but_not_cache_residency_charges() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let pin = cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    let clone = pin.clone();
    assert_eq!(quota.used_pages(), 1);
    assert!(matches!(
        cache.pin_folio_or_create(1, |folio| folio.complete(Ok(()))),
        Err(KError::StorageFull)
    ));
    cache.truncate_final().unwrap();
    assert_eq!(cache.nrpages(), 0);
    assert_eq!(quota.used_pages(), 0);
    let replacement = cache
        .pin_folio_or_create(1, |folio| folio.complete(Ok(())))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    drop(pin);
    assert_eq!(quota.used_pages(), 1);
    drop(clone);
    assert_eq!(
        quota.used_pages(),
        1,
        "detached owners do not affect cache quota"
    );
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    drop(replacement);
    drop(cache);
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn page_quota_limits_resident_entries_and_unpublished_pages_are_unaccounted() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let unpublished = Folio::new_zeroed(0).unwrap();
    assert_eq!(quota.used_pages(), 0);
    drop(unpublished);
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    assert_eq!(
        cache.with_folio_or_create(1, |folio| folio.complete(Ok(())), |_| Ok(())),
        Err(KError::StorageFull)
    );
    assert_eq!(cache.nrpages(), 1);
    drop(cache);
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn detached_external_reference_does_not_hold_cache_residency_quota() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    let external = cache.state.lock().entries[&0].clone();
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    assert_eq!(external.index(), 0);
    drop(external);
    assert_eq!(
        quota.used_pages(),
        1,
        "external physical owner is not a cache entry"
    );
}

#[def_test]
fn truncate_releases_cache_quota_while_batches_keep_physical_pages() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(
            0,
            |mut folio| {
                folio.data().fill(0x55);
                folio.complete(Ok(()))
            },
            |_| Ok(()),
        )
        .unwrap();
    let batch = cache.get_folios_contig(0, 1);
    assert_eq!(
        batch.with_read_folio(0, |folio| folio.data()[0]).unwrap(),
        Some(0x55)
    );
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    assert_eq!(
        batch.with_read_folio(0, |folio| folio.data()[0]).unwrap(),
        None
    );
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    drop(batch);
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn resident_lookup_does_not_charge_again_when_quota_is_full() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    cache
        .with_folio_or_create(0, |_| panic!("resident is initialized"), |_| Ok(()))
        .unwrap();
    assert_eq!(
        cache.with_folio_or_create(1, |folio| folio.complete(Ok(())), |_| Ok(())),
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
                folios.next().unwrap().complete(Ok(())).unwrap();
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
                        let mut folio = folios.next().unwrap();
                        folio.data().fill(0x5a);
                        folio.complete(Ok(())).unwrap();
                    })
                    .map(|_| ())
            } else {
                cache.with_folio_or_create(
                    0,
                    |mut folio| {
                        folio.data().fill(0x5a);
                        folio.complete(Ok(()))
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
fn mapping_drop_releases_cache_quota_before_retained_batch_drop() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    let batch = cache.get_folios_contig(0, 1);
    drop(cache);
    assert_eq!(batch.with_read_folio(0, |_| ()).unwrap(), None);
    assert_eq!(quota.used_pages(), 0);
    let replacement = MappingPages::with_residency(Some(quota.clone()));
    replacement
        .with_folio_or_create(0, |folio| folio.complete(Ok(())), |_| Ok(()))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    drop(batch);
    assert_eq!(quota.used_pages(), 1);
}

#[def_test(serial)]
fn cache_quota_is_released_while_managed_pte_keeps_physical_page() {
    use khal::paging::{MappingFlags, PageTable};
    use memaddr::VirtAddr;

    let initial_bytes = kalloc::global_allocator()
        .usages()
        .get(kalloc::UsageKind::PageCache);
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let pin = cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    let mut page_table = PageTable::try_new().unwrap();
    let mut page_table = page_table.modify();
    let vaddr = VirtAddr::from(0x1000usize);
    // SAFETY: the isolated table is never activated; tokens outlive its PTE.
    let mapping = unsafe {
        pin.map_readonly(
            &mut page_table,
            vaddr,
            MappingFlags::USER | MappingFlags::READ,
        )
    }
    .unwrap()
    .unwrap();
    let clone = mapping.clone();
    drop(cache);
    assert_eq!(quota.used_pages(), 0);
    assert_eq!(
        kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache),
        initial_bytes + PAGE_SIZE_4K
    );
    let replacement = MappingPages::with_residency(Some(quota.clone()));
    replacement
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    page_table.unmap(vaddr).unwrap();
    drop(mapping);
    assert_eq!(quota.used_pages(), 1);
    drop(clone);
    assert_eq!(quota.used_pages(), 1);
    page_table.finish();
    assert_eq!(
        quota.used_pages(),
        1,
        "only the replacement cache entry is charged"
    );
    assert_eq!(
        kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache),
        initial_bytes + PAGE_SIZE_4K,
        "retiring the old PTE releases its physical page independently of replacement quota"
    );
    replacement.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn failed_initialization_retains_only_the_existing_reservation_charge() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    for _ in 0..2 {
        assert!(matches!(
            cache.pin_folio_or_create(0, |_| Err(KError::Io)),
            Err(KError::Io)
        ));
        assert_eq!(quota.used_pages(), 1);
    }
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    cache
        .pin_folio_or_create(1, |folio| folio.complete(Ok(())))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn rejected_publications_neither_charge_nor_release_the_winner() {
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    let duplicate = cache
        .filemap_add_folio(Folio::new_zeroed(0).unwrap())
        .unwrap_err();
    let denied = cache
        .filemap_add_folio(Folio::new_zeroed(1).unwrap())
        .unwrap_err();
    assert_eq!(quota.used_pages(), 1);
    drop(duplicate);
    drop(denied);
    assert_eq!(quota.used_pages(), 1);
    cache.truncate_final().unwrap();
    assert_eq!(quota.used_pages(), 0);
    cache
        .pin_folio_or_create(1, |folio| folio.complete(Ok(())))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
}

#[def_test]
fn each_resident_cache_identity_holds_one_independent_charge() {
    let quota = Arc::new(TestResidency::new(2));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let original = cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    cache.truncate_final().unwrap();
    let replacement = cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    assert_eq!(quota.used_pages(), 1);
    drop(original);
    assert_eq!(quota.used_pages(), 1);
    drop(cache);
    assert_eq!(quota.used_pages(), 0);
    drop(replacement);
    assert_eq!(quota.used_pages(), 0);
}

#[def_test]
fn cache_detachment_releases_quota_under_the_mapping_index_lock() {
    use alloc::sync::Weak;

    use ksync::Mutex;

    struct LockCheckingResidency {
        mapping: Mutex<Weak<MappingPages>>,
        released: AtomicUsize,
    }

    impl CacheResidency for LockCheckingResidency {
        fn try_add_folio(&self) -> kerrno::KResult<()> {
            Ok(())
        }

        fn remove_folio(&self) {
            let mapping = self.mapping.try_lock().unwrap().upgrade().unwrap();
            assert!(mapping.state.try_lock().is_none());
            self.released.fetch_add(1, Ordering::AcqRel);
        }
    }

    let quota = Arc::new(LockCheckingResidency {
        mapping: Mutex::new(Weak::new()),
        released: AtomicUsize::new(0),
    });
    let cache = Arc::new(MappingPages::with_residency(Some(quota.clone())));
    *quota.mapping.lock() = Arc::downgrade(&cache);
    let pin = cache
        .pin_folio_or_create(0, |folio| folio.complete(Ok(())))
        .unwrap();
    cache.truncate_final().unwrap();
    assert_eq!(quota.released.load(Ordering::Acquire), 1);
    drop(pin);
    assert_eq!(quota.released.load(Ordering::Acquire), 1);
}

#[def_test(serial)]
fn detached_read_token_keeps_physical_page_after_quota_release() {
    for outcome in 0..3 {
        let quota = Arc::new(TestResidency::new(1));
        let initial_bytes = kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache);
        let cache = MappingPages::with_residency(Some(quota.clone()));
        let mut pending = None;
        cache
            .readahead_folios(0..1, |folios| pending = folios.next())
            .unwrap();
        let page = Arc::downgrade(&cache.state.lock().entries[&0]);
        drop(cache);
        assert_eq!(page.strong_count(), 1);
        assert_eq!(quota.used_pages(), 0);
        assert_eq!(
            kalloc::global_allocator()
                .usages()
                .get(kalloc::UsageKind::PageCache),
            initial_bytes + PAGE_SIZE_4K
        );
        {
            let _irq = kirq::context::test_support::ScopedHardIrqContext::enter();
            let pending = pending.take().unwrap();
            match outcome {
                0 => pending.complete(Ok(())).unwrap(),
                1 => core::assert_eq!(pending.complete(Err(KError::Io)), Err(KError::Io)),
                _ => drop(pending),
            }
        }
        assert!(page.upgrade().is_none());
        assert_eq!(quota.used_pages(), 0);
        assert_eq!(
            kalloc::global_allocator()
                .usages()
                .get(kalloc::UsageKind::PageCache),
            initial_bytes
        );
    }
}

#[def_test(serial)]
fn pte_and_retirement_keep_physical_page_after_cache_quota_release() {
    use khal::paging::{MappingFlags, PageTable};
    use memaddr::VirtAddr;

    let initial_bytes = kalloc::global_allocator()
        .usages()
        .get(kalloc::UsageKind::PageCache);
    let quota = Arc::new(TestResidency::new(1));
    let cache = MappingPages::with_residency(Some(quota.clone()));
    let pin = cache
        .pin_folio_or_create(0, |read| read.complete(Ok(())))
        .unwrap();
    let object = Arc::downgrade(&cache.state.lock().entries[&0]);
    let mut table = PageTable::try_new().unwrap();
    let mut table = table.modify();
    let address = VirtAddr::from(0x30_000usize);
    // SAFETY: this inactive test table has no users; the initialized page and
    // file extent are private to this fixture. Publication gives the PTE a hold.
    let token =
        unsafe { pin.map_readonly(&mut table, address, MappingFlags::USER | MappingFlags::READ) }
            .unwrap()
            .unwrap();
    let paddr = table.query(address).unwrap().0;
    drop(cache);
    drop(token);
    assert!(
        object.upgrade().is_none(),
        "the PTE retains the page, not the Folio"
    );
    assert_eq!(quota.used_pages(), 0);
    assert_eq!(
        kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache),
        initial_bytes + PAGE_SIZE_4K
    );
    assert_eq!(table.query(address).unwrap().0, paddr);
    table.unmap(address).unwrap();
    assert_eq!(
        kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache),
        initial_bytes + PAGE_SIZE_4K,
        "retirement still owns the physical page"
    );
    table.finish();
    assert_eq!(
        kalloc::global_allocator()
            .usages()
            .get(kalloc::UsageKind::PageCache),
        initial_bytes
    );
}
