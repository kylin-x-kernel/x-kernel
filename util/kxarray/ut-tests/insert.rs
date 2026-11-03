// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::BTreeMap;
use core::sync::atomic::AtomicUsize;

use unittest::{assert, assert_eq, def_test};

use super::*;

struct FailingAllocator<'a, P: EntryOwner> {
    array: &'a XArray<P>,
    remaining: usize,
    attempts: usize,
}

#[def_test]
fn invalid_flags_reject_all_updates_before_publication() {
    let array = XArray::<Arc<usize>>::new();
    let offered = Arc::new(7);
    let invalid = AllocFlags::from_bits_retain(1 << 31);
    // SAFETY: eligible task and primitive Arc release; invalid flags reject first.
    let (error, returned) =
        unsafe { array.insert_with_flags(0, offered.clone(), invalid) }.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert!(Arc::ptr_eq(&returned, &offered));
    // SAFETY: same task and release-chain contract; no owner may be published.
    let (error, returned) = unsafe { array.set_with_flags(0, returned, invalid) }.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert!(Arc::ptr_eq(&returned, &offered));
    assert_eq!(
        // SAFETY: same eligible task; invalid flags reject before RCU preflight.
        unsafe { array.remove_with_flags(0, invalid) },
        Err(KError::InvalidInput)
    );
    assert!(array.root.load(Ordering::Acquire).is_null());
    assert!(
        // SAFETY: initialized task outside read scopes; primitive Arc payload
        // has a nonblocking release chain and NOWAIT excludes sleeping reclaim.
        unsafe { array.insert_with_flags(0, Arc::new(11), AllocFlags::NOWAIT) }.is_ok()
    );
    for index in [0, 1] {
        // SAFETY: the same task and primitive release contract applies; unknown
        // flags must reject both occupied and empty slots of the existing leaf.
        let (error, returned) =
            unsafe { array.insert_with_flags(index, offered.clone(), invalid) }.unwrap_err();
        assert_eq!(error, KError::InvalidInput);
        assert!(Arc::ptr_eq(&returned, &offered));
        // SAFETY: eligible watched task, permanent RCU service and nonblocking
        // Arc destruction; invalid flags must reject before any replacement.
        let (error, returned) =
            unsafe { array.set_with_flags(index, returned, invalid) }.unwrap_err();
        assert_eq!(error, KError::InvalidInput);
        assert!(Arc::ptr_eq(&returned, &offered));
        assert_eq!(
            // SAFETY: watched task outside read scopes with permanent service
            // availability; invalid flags reject before either slot is withdrawn.
            unsafe { array.remove_with_flags(index, invalid) },
            Err(KError::InvalidInput)
        );
    }
    assert_eq!(
        krcu::with_read(|scope| (array.read(scope, 0).copied(), array.read(scope, 1).copied())),
        (Some(11), None)
    );
}

impl<P: EntryOwner> Allocator for FailingAllocator<'_, P> {
    fn allocate<T>(&mut self, value: T) -> KResult<KBox<T>> {
        if self.array.writers.try_lock().is_none() {
            return Err(KError::ResourceBusy);
        }
        self.attempts += 1;
        if self.remaining == 0 {
            return Err(KError::NoMemory);
        }
        self.remaining -= 1;
        KBox::new_nowait(value)
    }
}

#[def_test]
fn partial_reservation_failure_preserves_root_and_offered_owner() {
    for remaining in 0..2 * (MAX_HEIGHT as usize - 1) {
        let array = XArray::<Arc<usize>>::new();
        // SAFETY: sleepable task, primitive Arc payload and nonblocking release.
        assert!(unsafe { array.insert(0, Arc::new(7)) }.is_ok());
        let root = array.root.load(Ordering::Acquire);
        let offered = Arc::new(9);
        let mut allocator = FailingAllocator {
            array: &array,
            remaining,
            attempts: 0,
        };
        // SAFETY: task outside reader scopes; allocator uses NOWAIT without locks.
        let (error, returned) =
            unsafe { array.insert_with_allocator(usize::MAX, offered.clone(), &mut allocator) }
                .unwrap_err();
        assert_eq!(error, KError::NoMemory);
        assert!(Arc::ptr_eq(&returned, &offered));
        assert_eq!(allocator.attempts, remaining + 1);
        assert_eq!(array.root.load(Ordering::Acquire), root);
        let values = krcu::with_read(|scope| {
            (
                array.read(scope, 0).copied(),
                array.read(scope, usize::MAX).copied(),
            )
        });
        assert_eq!(values, (Some(7), None));
        drop(returned);
        assert_eq!(Arc::strong_count(&offered), 1);
        // SAFETY: same eligible task and primitive release chain; retry is independent.
        assert!(unsafe { array.insert(usize::MAX, offered.clone()) }.is_ok());
    }
}

#[def_test]
fn retirement_failure_preserves_association_for_set_and_remove() {
    let array = XArray::<Arc<usize>>::new();
    let old = Arc::new(7);
    let offered = Arc::new(9);
    // SAFETY: sleepable task, primitive Arc payload and nonblocking release.
    assert!(unsafe { array.insert(0, old.clone()) }.is_ok());
    let mut allocator = FailingAllocator {
        array: &array,
        remaining: 0,
        attempts: 0,
    };
    // SAFETY: eligible watched task outside scopes/locks, permanent RCU service.
    let (error, returned) =
        unsafe { array.set_with_allocator(0, offered.clone(), &mut allocator) }.unwrap_err();
    assert_eq!(error, KError::NoMemory);
    assert!(Arc::ptr_eq(&returned, &offered));
    assert_eq!(
        // SAFETY: same RCU context; failure must precede withdrawal.
        unsafe { array.remove_with_allocator(0, &mut allocator) },
        Err(KError::NoMemory)
    );
    assert_eq!(allocator.attempts, 2);
    assert_eq!(
        krcu::with_read(|scope| array.read(scope, 0).copied()),
        Some(7)
    );
    assert_eq!(Arc::strong_count(&old), 2);
    // SAFETY: same task and owner contract; conflict must not allocate.
    let (error, returned) =
        unsafe { array.insert_with_allocator(0, returned, &mut allocator) }.unwrap_err();
    assert_eq!(error, KError::AlreadyExists);
    assert!(Arc::ptr_eq(&returned, &offered));
    assert_eq!(allocator.attempts, 2);
    assert_eq!(
        // SAFETY: same eligible context; a missing removal needs no retirement storage.
        unsafe { array.remove_with_allocator(1, &mut allocator) },
        Ok(false)
    );
    assert_eq!(allocator.attempts, 2);
}

#[def_test]
fn set_missing_key_reservation_failure_returns_owner() {
    let array = XArray::<Arc<usize>>::new();
    let offered = Arc::new(7);
    let mut allocator = FailingAllocator {
        array: &array,
        remaining: 1,
        attempts: 0,
    };
    // SAFETY: eligible task, primitive release chain; allocations occur unlocked.
    let (error, returned) =
        unsafe { array.set_with_allocator(usize::MAX, offered.clone(), &mut allocator) }
            .unwrap_err();
    assert_eq!(error, KError::NoMemory);
    assert!(Arc::ptr_eq(&returned, &offered));
    assert!(array.root.load(Ordering::Acquire).is_null());
    assert_eq!(allocator.attempts, 2);
}

impl<P: EntryOwner> XArray<P> {
    /// Observes the first new child link before its payload is installed.
    ///
    /// The one-shot observer runs with the writer lock held and must neither
    /// allocate, wait nor mutate this array. The child may still be beneath an
    /// unpublished root. Available only in kernel unittest builds.
    #[doc(hidden)]
    pub fn set_publication_hook_for_test(&self, hook_fn: fn(&Self, usize)) {
        *self.publication_hook.lock() = Some(hook_fn);
    }

    /// Installs a one-shot unlocked checkpoint after `set` prepares storage.
    ///
    /// The observer runs before reinspection, for either nodes or a retirement
    /// record. It must return with the caller's execution context restored.
    /// Available only in kernel unittest builds.
    #[doc(hidden)]
    pub fn set_preparation_hook_for_test(&self, hook_fn: fn(&Self, usize)) {
        *self.preparation_hook.lock() = Some(hook_fn);
    }

    /// Runs a nonblocking test observation while holding the real writer lock.
    ///
    /// The observer must not mutate this array, allocate or wait. This fixes
    /// writer exclusion for kernel interrupt fixtures without adding a hook
    /// to production mutation paths. Available only in kernel unittest builds.
    #[doc(hidden)]
    pub fn with_writer_locked_for_test<R>(&self, observe_fn: impl FnOnce() -> R) -> R {
        let _guard = self.writers.lock();
        observe_fn()
    }
}

pub(super) type PreparationHook<P> = fn(&XArray<P>, usize);
pub(super) type PublicationHook<P> = fn(&XArray<P>, usize);

pub(super) fn after_child_publication<P: EntryOwner>(array: &XArray<P>, index: usize) {
    let hook = array.publication_hook.lock().take();
    if let Some(hook_fn) = hook {
        hook_fn(array, index);
    }
}

pub(super) fn after_set_preparation<P: EntryOwner>(array: &XArray<P>, index: usize) {
    let hook = array.preparation_hook.lock().take();
    if let Some(hook_fn) = hook {
        hook_fn(array, index);
    }
}

#[def_test]
fn full_width_sparse_growth_preserves_existing_entries() {
    let array = XArray::<KBox<usize>>::new();
    let mut model = BTreeMap::new();
    for index in [0, 63, 64, 4095, 4096, usize::MAX, 1 << (usize::BITS - 1), 1] {
        assert!(
            // SAFETY: task-context test; usize has no destructor and kernel KBox
            // deallocation is nonblocking. No IRQ/NMI or reader scope is active.
            unsafe {
                array.insert_with_flags(index, KBox::new_nowait(index).unwrap(), AllocFlags::NOWAIT)
            }
            .is_ok()
        );
        model.insert(index, index);
        let agrees = krcu::with_read(|scope| {
            model
                .iter()
                .all(|(key, value)| array.read(scope, *key) == Some(value))
                && array.read(scope, 2).is_none()
        });
        assert!(agrees);
    }
    let duplicate = KBox::new_nowait(17).unwrap();
    let original = core::ptr::from_ref(&*duplicate);
    // SAFETY: same task context and nonblocking usize release contract.
    let (error, rejected) =
        unsafe { array.insert_with_flags(64, duplicate, AllocFlags::NOWAIT) }.unwrap_err();
    assert_eq!(error, KError::AlreadyExists);
    assert_eq!(core::ptr::from_ref(&*rejected), original);
    assert_eq!(*rejected, 17);
    assert_eq!(
        krcu::with_read(|scope| array.read(scope, 64).copied()),
        Some(64)
    );
}

struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn arc_lookup_borrows_without_counting_and_can_acquire_an_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let value = Arc::new(DropProbe(drops.clone()));
    let array = XArray::<Arc<DropProbe>>::new();
    assert!(
        // SAFETY: release only increments an atomic and drops Arc allocations;
        // allocation/free is nonblocking and this test runs in task context.
        unsafe { array.insert_with_flags(usize::MAX, value.clone(), AllocFlags::NOWAIT) }.is_ok()
    );
    assert_eq!(Arc::strong_count(&value), 2);
    let borrowed = krcu::with_read(|scope| {
        array.read(scope, usize::MAX).is_some() && Arc::strong_count(&value) == 2
    });
    assert!(borrowed);
    let retained = krcu::with_read(|scope| array.get_owned(scope, usize::MAX)).unwrap();
    assert_eq!(Arc::strong_count(&value), 3);
    drop(array);
    assert_eq!(Arc::strong_count(&value), 2);
    drop(value);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(retained);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn alignment_one_and_zero_sized_payloads_need_no_pointer_tags() {
    let bytes = XArray::<KBox<u8>>::new();
    let units = XArray::<KBox<()>>::new();
    for index in [0, 64, usize::MAX] {
        assert!(
            // SAFETY: primitive release and allocator free are nonblocking; the
            // fixture inserts from task context without holding an exclusion guard.
            unsafe {
                bytes.insert_with_flags(index, KBox::new_nowait(9).unwrap(), AllocFlags::NOWAIT)
            }
            .is_ok()
        );
        assert!(
            // SAFETY: unit has no destructor and ZST KBox owns no backing allocation.
            unsafe {
                units.insert_with_flags(index, KBox::new_nowait(()).unwrap(), AllocFlags::NOWAIT)
            }
            .is_ok()
        );
    }
    assert!(krcu::with_read(|scope| {
        [0, 64, usize::MAX].iter().all(|index| {
            bytes.read(scope, *index) == Some(&9) && units.read(scope, *index).is_some()
        })
    }));
}

#[def_test]
fn exclusive_drop_releases_every_box_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let array = XArray::<KBox<DropProbe>>::new();
    for index in [0, 1, 64, usize::MAX] {
        assert!(
            // SAFETY: atomic-only destructor and nonblocking KBox/Arc frees, inserted
            // from task context; no callback depends on this test's future progress.
            unsafe {
                array.insert_with_flags(
                    index,
                    KBox::new_nowait(DropProbe(drops.clone())).unwrap(),
                    AllocFlags::NOWAIT,
                )
            }
            .is_ok()
        );
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(array);
    assert_eq!(drops.load(Ordering::Relaxed), 4);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[def_test]
fn replacement_releases_each_displaced_box_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let array = XArray::<KBox<DropProbe>>::new();
    for _ in 0..3 {
        assert!(
            // SAFETY: watched task outside any read scope; the destructor uses only
            // an atomic increment and nonblocking allocator release. All references
            // needed by callbacks are owned independently through Arc.
            unsafe {
                array.set_with_flags(
                    17,
                    KBox::new_nowait(DropProbe(drops.clone())).unwrap(),
                    AllocFlags::NOWAIT,
                )
            }
            .is_ok()
        );
    }
    assert!(krcu::with_read(|scope| array.read(scope, 17).is_some()));
    // The container need not survive its detached callbacks.
    drop(array);
    // SAFETY: watched sleepable task, no reader/lock held and no callback waits
    // for this test. The permanent boot service remains available throughout.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(drops.load(Ordering::Relaxed), 3);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[def_test]
fn rejected_replacement_preserves_input_and_existing_association() {
    let array = XArray::<KBox<usize>>::new();
    assert!(
        // SAFETY: primitive release, initialized task context without exclusion.
        unsafe { array.set_with_flags(0, KBox::new_nowait(1).unwrap(), AllocFlags::NOWAIT,) }
            .is_ok()
    );
    let offered = KBox::new_nowait(2).unwrap();
    let original = core::ptr::from_ref(&*offered);
    // Masking IRQs makes preflight reject replacement before publication.
    let result = {
        let _irq = kspin::IrqSave::new();
        // SAFETY: allocation-permitted task and callback-safe payload. The
        // runtime preflight rejects masked IRQs without withdrawing ownership.
        unsafe { array.set_with_flags(0, offered, AllocFlags::NOWAIT) }
    };
    let (error, returned) = result.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(core::ptr::from_ref(&*returned), original);
    assert_eq!(
        krcu::with_read(|scope| array.read(scope, 0).copied()),
        Some(1)
    );
}

#[def_test]
fn replacement_drops_only_the_arrays_arc_reference() {
    let old = Arc::new(7_usize);
    let array = XArray::<Arc<usize>>::new();
    // SAFETY: primitive Arc payload, ordinary watched task context.
    assert!(unsafe { array.set_with_flags(0, old.clone(), AllocFlags::NOWAIT) }.is_ok());
    let retained = krcu::with_read(|scope| array.get_owned(scope, 0)).unwrap();
    // SAFETY: same context and nonblocking release contract.
    assert!(unsafe { array.set_with_flags(0, Arc::new(8), AllocFlags::NOWAIT) }.is_ok());
    // SAFETY: watched sleepable task outside scopes/locks; callbacks release
    // only independently owned primitive payloads, never waiting for this test.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(Arc::strong_count(&old), 2);
    assert_eq!(*retained, 7);
    assert_eq!(
        krcu::with_read(|scope| array.read(scope, 0).copied()),
        Some(8)
    );
}
