// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::*;

pub(super) struct Probe<'a>(pub(super) &'a AtomicUsize);

impl Drop for Probe<'_> {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn shared_identity_and_counts() {
    let owned = KArc::new_nowait(42).unwrap();
    let cloned = owned.clone();
    let other = KArc::new_nowait(42).unwrap();
    assert!(KArc::ptr_eq(&owned, &cloned));
    assert!(!KArc::ptr_eq(&owned, &other));
    assert_eq!(KArc::strong_count(&owned), 2);
    assert_eq!(KArc::weak_count(&owned), 0);
    assert_eq!(*cloned, 42);
    assert_eq!(*owned.as_ref(), 42);
    drop(cloned);
    assert_eq!(KArc::strong_count(&owned), 1);
}

pub(super) fn weak_retains_storage_not_payload() {
    let drops = AtomicUsize::new(0);
    let owned = KArc::new_nowait(Probe(&drops)).unwrap();
    let weak = KArc::downgrade(&owned);
    let cloned = weak.clone();
    assert!(KWeak::ptr_eq(&weak, &cloned));
    assert_eq!(KArc::weak_count(&owned), 2);
    let upgraded = weak.upgrade().unwrap();
    assert!(KArc::ptr_eq(&owned, &upgraded));
    drop(owned);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(upgraded);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(weak.upgrade().is_none());
    let after_death = weak.clone();
    drop((weak, cloned, after_death));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

pub(super) fn empty_weak_cannot_upgrade() {
    let empty = KWeak::<u32>::new();
    let default = KWeak::default();
    assert!(empty.upgrade().is_none());
    assert!(empty.clone().upgrade().is_none());
    assert!(KWeak::ptr_eq(&empty, &default));
}

pub(super) fn extraction_invalidates_existing_weak() {
    let drops = AtomicUsize::new(0);
    let owned = KArc::new_nowait(Probe(&drops)).unwrap();
    let weak = KArc::downgrade(&owned);
    let value = KArc::try_unwrap(owned).ok().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

pub(super) fn failed_extraction_preserves_owner() {
    let owned = KArc::new_nowait(17).unwrap();
    let cloned = owned.clone();
    let returned = KArc::try_unwrap(owned).unwrap_err();
    assert!(KArc::ptr_eq(&cloned, &returned));
    drop(cloned);
    assert_eq!(KArc::try_unwrap(returned).unwrap(), 17);
}

pub(super) fn reservation_failure_drops_input() {
    let drops = AtomicUsize::new(0);
    for flags in [AllocFlags::KERNEL, AllocFlags::NOWAIT] {
        let result = KArc::new_in(Probe(&drops), SharedAllocator { flags, fail: true });
        assert!(matches!(result, Err(KError::NoMemory)));
    }
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

pub(super) fn invalid_flags_reject_input() {
    let flags = AllocFlags::from_bits_retain(1 << 31);
    let drops = AtomicUsize::new(0);
    let result = KArc::new_in(Probe(&drops), SharedAllocator { flags, fail: true });
    assert!(matches!(result, Err(KError::InvalidInput)));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(matches!(
        KArc::new_with_flags((), flags),
        Err(KError::InvalidInput)
    ));
}

pub(super) fn zero_sized_values_still_need_shared_storage() {
    let result = KArc::new_in(
        (),
        SharedAllocator {
            flags: AllocFlags::NOWAIT,
            fail: true,
        },
    );
    assert!(matches!(result, Err(KError::NoMemory)));
    let owned = KArc::new_nowait(()).unwrap();
    let weak = KArc::downgrade(&owned);
    drop(owned);
    assert!(weak.upgrade().is_none());
}

pub(super) fn aligned_value_preserves_alignment() {
    #[repr(align(256))]
    struct Aligned(u32);
    let owned = KArc::new_nowait(Aligned(9)).unwrap();
    assert_eq!(core::ptr::from_ref(&*owned) as usize % 256, 0);
    assert_eq!(owned.as_ref().0, 9);
    let weak = KArc::downgrade(&owned);
    drop(owned);
    assert!(weak.upgrade().is_none());
}

pub(super) fn shared_handles_are_send_and_sync_for_atomic_payloads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<KArc<AtomicUsize>>();
    assert_send_sync::<KWeak<AtomicUsize>>();
}

pub(super) fn raw_transfer_retains_one_strong_reference() {
    for flags in [AllocFlags::KERNEL, AllocFlags::NOWAIT] {
        let drops = AtomicUsize::new(0);
        let owned = KArc::new_with_flags(Probe(&drops), flags).unwrap();
        let weak = KArc::downgrade(&owned);
        let address = KArc::as_ptr(&owned);
        let pointer = KArc::into_raw(owned);
        assert_eq!(pointer, address);
        let upgraded = weak.upgrade().unwrap();
        assert_eq!(KArc::strong_count(&upgraded), 2);
        drop(upgraded);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        // SAFETY: pointer is the single unrecovered reference transferred above.
        let recovered = unsafe { KArc::from_raw(pointer) };
        assert_eq!(KArc::strong_count(&recovered), 1);
        let recovered_weak = KArc::downgrade(&recovered);
        drop(weak);
        drop(recovered);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(recovered_weak.upgrade().is_none());
        drop(recovered_weak);
    }
}

pub(super) fn independent_raw_transfers_preserve_extraction() {
    let owned = KArc::new_nowait(42).unwrap();
    let first = KArc::into_raw(owned.clone());
    let second = KArc::into_raw(owned);
    assert_eq!(first, second);
    // SAFETY: first and second each represent a distinct transferred reference.
    let first = unsafe { KArc::from_raw(first) };
    // SAFETY: second's separately transferred reference is still unclaimed.
    let second = unsafe { KArc::from_raw(second) };
    let returned = KArc::try_unwrap(first).unwrap_err();
    drop(second);
    assert_eq!(KArc::try_unwrap(returned).unwrap(), 42);
}

pub(super) fn raw_transfer_preserves_aligned_zst() {
    #[repr(align(256))]
    struct Aligned;
    let owned = KArc::new_nowait(Aligned).unwrap();
    let pointer = KArc::into_raw(owned);
    assert_eq!(pointer as usize % 256, 0);
    // SAFETY: pointer is the original, still-unclaimed reference to Aligned.
    let recovered = unsafe { KArc::from_raw(pointer) };
    assert_eq!(KArc::as_ptr(&recovered), pointer);
    let weak = KArc::downgrade(&recovered);
    drop(recovered);
    assert!(weak.upgrade().is_none());
    drop(weak);
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
struct LinkedProbe<'a> {
    pending: intrusive_collections::LinkedListLink,
    ready: intrusive_collections::LinkedListLink,
    _probe: Probe<'a>,
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
#[derive(Clone)]
struct ProbeAdapter<'a, const READY: bool> {
    links: <intrusive_collections::LinkedListLink as intrusive_collections::DefaultLinkOps>::Ops,
    pointers: crate::KArcPointerOps<LinkedProbe<'a>>,
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
impl<const READY: bool> ProbeAdapter<'_, READY> {
    const OFFSET: usize = if READY {
        core::mem::offset_of!(LinkedProbe<'_>, ready)
    } else {
        core::mem::offset_of!(LinkedProbe<'_>, pending)
    };

    fn new() -> Self {
        Self {
            links: <intrusive_collections::LinkedListLink as intrusive_collections::DefaultLinkOps>::NEW,
            pointers: crate::KArcPointerOps::new(),
        }
    }
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
// SAFETY: both conversions use the same selected field offset in LinkedProbe.
unsafe impl<'a, const READY: bool> intrusive_collections::Adapter for ProbeAdapter<'a, READY> {
    type LinkOps =
        <intrusive_collections::LinkedListLink as intrusive_collections::DefaultLinkOps>::Ops;
    type PointerOps = crate::KArcPointerOps<LinkedProbe<'a>>;

    unsafe fn get_value(
        &self,
        link: NonNull<intrusive_collections::LinkedListLink>,
    ) -> *const LinkedProbe<'a> {
        // SAFETY: Adapter supplies the selected link of a live LinkedProbe.
        unsafe { link.as_ptr().cast::<u8>().sub(Self::OFFSET).cast() }
    }

    unsafe fn get_link(
        &self,
        value: *const LinkedProbe<'a>,
    ) -> NonNull<intrusive_collections::LinkedListLink> {
        // SAFETY: value is a live allocation; OFFSET identifies its link field.
        let link = unsafe { value.cast::<u8>().add(Self::OFFSET).cast_mut().cast() };
        // SAFETY: a field in the supplied live allocation cannot be null.
        unsafe { NonNull::new_unchecked(link) }
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.links
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.links
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointers
    }
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
fn linked_probe(drops: &AtomicUsize) -> KArc<LinkedProbe<'_>> {
    KArc::new_nowait(LinkedProbe {
        pending: intrusive_collections::LinkedListLink::new(),
        ready: intrusive_collections::LinkedListLink::new(),
        _probe: Probe(drops),
    })
    .unwrap()
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
pub(super) fn list_rejection_and_removal_preserve_ownership() {
    extern crate std;

    let drops = AtomicUsize::new(0);
    let owned = linked_probe(&drops);
    let mut pending = intrusive_collections::LinkedList::new(ProbeAdapter::<false>::new());
    let mut other = intrusive_collections::LinkedList::new(ProbeAdapter::<false>::new());
    pending.push_back(owned.clone());
    let rejected = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        other.push_back(owned.clone());
    }));
    assert!(rejected.is_err());
    assert_eq!(KArc::strong_count(&owned), 2);
    assert!(other.is_empty());
    // SAFETY: this exact node is still linked in pending through its pending field.
    let detached = unsafe { pending.cursor_mut_from_ptr(KArc::as_ptr(&owned)) }
        .remove()
        .unwrap();
    assert!(KArc::ptr_eq(&owned, &detached));
    assert!(pending.is_empty());
    other.push_back(detached);
    drop(owned);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(other.pop_front().unwrap());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
pub(super) fn independent_links_and_teardown_release_each_reference() {
    let drops = AtomicUsize::new(0);
    let owned = linked_probe(&drops);
    let weak = KArc::downgrade(&owned);
    let mut pending = intrusive_collections::LinkedList::new(ProbeAdapter::<false>::new());
    let mut ready = intrusive_collections::LinkedList::new(ProbeAdapter::<true>::new());
    pending.push_back(owned.clone());
    ready.push_back(owned);
    drop(pending.pop_front().unwrap());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert!(weak.upgrade().is_some());
    drop(ready);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(weak.upgrade().is_none());
    drop(weak);
}

#[cfg(all(test, not(unittest), feature = "intrusive-collections"))]
pub(super) fn library_take_transfers_owners_without_destruction() {
    let drops = AtomicUsize::new(0);
    let owned = linked_probe(&drops);
    let mut list = intrusive_collections::LinkedList::new(ProbeAdapter::<false>::new());
    list.push_back(owned.clone());
    let detached = list.take();
    assert!(list.is_empty());
    drop(list);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(KArc::strong_count(&owned), 2);
    drop(detached);
    assert_eq!(KArc::strong_count(&owned), 1);
    let mut reused = intrusive_collections::LinkedList::new(ProbeAdapter::<false>::new());
    reused.push_back(owned);
    drop(reused);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
