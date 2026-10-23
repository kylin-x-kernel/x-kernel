// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::collections::BTreeMap;
use core::sync::atomic::AtomicUsize;

use unittest::{assert, assert_eq, def_test};

use super::*;

impl<P: EntryOwner> XArray<P> {
    /// Observes the first new child link before its payload is installed.
    ///
    /// The one-shot observer runs with the writer lock held and must neither
    /// allocate, wait nor mutate this array. The child may still be beneath an
    /// unpublished root. Available only in kernel unittest builds.
    pub(crate) fn set_publication_hook_for_test(&self, hook_fn: fn(&Self, usize)) {
        *self.publication_hook.lock() = Some(hook_fn);
    }

    /// Installs a one-shot unlocked checkpoint after `set` prepares storage.
    ///
    /// The observer runs before reinspection, for either nodes or a retirement
    /// record. It must return with the caller's execution context restored.
    /// Available only in kernel unittest builds.
    pub(crate) fn set_preparation_hook_for_test(&self, hook_fn: fn(&Self, usize)) {
        *self.preparation_hook.lock() = Some(hook_fn);
    }

    /// Runs a nonblocking test observation while holding the real writer lock.
    ///
    /// The observer must not mutate this array, allocate or wait. This fixes
    /// writer exclusion for kernel interrupt fixtures without adding a hook
    /// to production mutation paths. Available only in kernel unittest builds.
    pub(crate) fn with_writer_locked_for_test<R>(&self, observe_fn: impl FnOnce() -> R) -> R {
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
    let array = XArray::<Box<usize>>::new();
    let mut model = BTreeMap::new();
    for index in [0, 63, 64, 4095, 4096, usize::MAX, 1 << (usize::BITS - 1), 1] {
        // SAFETY: task-context test; usize has no destructor and kernel Box
        // deallocation is nonblocking. No IRQ/NMI or reader scope is active.
        assert!(unsafe { array.insert(index, Box::new(index)) }.is_ok());
        model.insert(index, index);
        let agrees = krcu::with_read(|scope| {
            model.iter().all(|(key, value)| array.read(scope, *key) == Some(value))
                && array.read(scope, 2).is_none()
        });
        assert!(agrees);
    }
    let duplicate = Box::new(17);
    let original = core::ptr::from_ref(&*duplicate);
    // SAFETY: same task context and nonblocking usize release contract.
    let rejected = unsafe { array.insert(64, duplicate) }.unwrap_err();
    assert_eq!(core::ptr::from_ref(&*rejected), original);
    assert_eq!(*rejected, 17);
    assert_eq!(krcu::with_read(|scope| array.read(scope, 64).copied()), Some(64));
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
    // SAFETY: release only increments an atomic and drops Arc allocations;
    // allocation/free is nonblocking and this test runs in task context.
    assert!(unsafe { array.insert(usize::MAX, value.clone()) }.is_ok());
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
    let bytes = XArray::<Box<u8>>::new();
    let units = XArray::<Box<()>>::new();
    for index in [0, 64, usize::MAX] {
        // SAFETY: primitive release and allocator free are nonblocking; the
        // fixture inserts from task context without holding an exclusion guard.
        assert!(unsafe { bytes.insert(index, Box::new(9)) }.is_ok());
        // SAFETY: unit has no destructor and ZST Box owns no backing allocation.
        assert!(unsafe { units.insert(index, Box::new(())) }.is_ok());
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
    let array = XArray::<Box<DropProbe>>::new();
    for index in [0, 1, 64, usize::MAX] {
        // SAFETY: atomic-only destructor and nonblocking Box/Arc frees, inserted
        // from task context; no callback depends on this test's future progress.
        assert!(unsafe { array.insert(index, Box::new(DropProbe(drops.clone()))) }.is_ok());
    }
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(array);
    assert_eq!(drops.load(Ordering::Relaxed), 4);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[def_test]
fn replacement_releases_each_displaced_box_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let array = XArray::<Box<DropProbe>>::new();
    for _ in 0..3 {
        // SAFETY: watched task outside any read scope; the destructor uses only
        // an atomic increment and nonblocking allocator release. All references
        // needed by callbacks are owned independently through Arc.
        assert!(unsafe { array.set(17, Box::new(DropProbe(drops.clone()))) }.is_ok());
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
    let array = XArray::<Box<usize>>::new();
    // SAFETY: primitive release, initialized task context without exclusion.
    assert!(unsafe { array.set(0, Box::new(1)) }.is_ok());
    let offered = Box::new(2);
    let original = core::ptr::from_ref(&*offered);
    // Masking IRQs makes preflight reject replacement before publication.
    let result = {
        let _irq = kspin::IrqSave::new();
        // SAFETY: allocation-permitted task and callback-safe payload. The
        // runtime preflight rejects masked IRQs without withdrawing ownership.
        unsafe { array.set(0, offered) }
    };
    let (error, returned) = result.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(core::ptr::from_ref(&*returned), original);
    assert_eq!(krcu::with_read(|scope| array.read(scope, 0).copied()), Some(1));
}

#[def_test]
fn replacement_drops_only_the_arrays_arc_reference() {
    let old = Arc::new(7_usize);
    let array = XArray::<Arc<usize>>::new();
    // SAFETY: primitive Arc payload, ordinary watched task context.
    assert!(unsafe { array.set(0, old.clone()) }.is_ok());
    let retained = krcu::with_read(|scope| array.get_owned(scope, 0)).unwrap();
    // SAFETY: same context and nonblocking release contract.
    assert!(unsafe { array.set(0, Arc::new(8)) }.is_ok());
    // SAFETY: watched sleepable task outside scopes/locks; callbacks release
    // only independently owned primitive payloads, never waiting for this test.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(Arc::strong_count(&old), 2);
    assert_eq!(*retained, 7);
    assert_eq!(krcu::with_read(|scope| array.read(scope, 0).copied()), Some(8));
}
