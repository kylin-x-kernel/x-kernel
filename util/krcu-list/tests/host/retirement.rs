// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use super::*;

struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn embedded_callback_recovers_original_allocation_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let raw = KBox::into_raw(KBox::new_nowait(Node::new(Probe(drops.clone()))).unwrap());
    // SAFETY: raw originates from the complete live allocation, not a reborrow.
    let callback = unsafe { &raw mut (*raw).callback };
    // SAFETY: fixture never publishes the node, so there are no readers or
    // executor handles. It owns the single invocation right.
    unsafe { release::<Probe>(callback) };
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[test]
fn rejected_retirement_keeps_payload_alive() {
    let drops = Arc::new(AtomicUsize::new(0));
    let raw = KBox::into_raw(KBox::new_nowait(Node::new(Probe(drops.clone()))).unwrap());
    // SAFETY: project the embedded record from its original KBox pointer.
    let callback = unsafe { &raw mut (*raw).callback };
    // SAFETY: live unique unqueued record; cancellation must retain it.
    unsafe { retain_rejected(callback) };
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    // SAFETY: test holds the retained ownership, with no publication/readers.
    unsafe { release::<Probe>(callback) };
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

use core::cell::Cell;

struct RawDropProbe<'a>(&'a Cell<usize>);

impl Drop for RawDropProbe<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn raw_owner_roundtrip_preserves_address_and_drops_once() {
    let drops = Cell::new(0);
    let owned = KBox::new_nowait(RawDropProbe(&drops)).unwrap();
    let address = &*owned as *const RawDropProbe<'_>;
    let raw = KBox::into_raw(owned);
    assert_eq!(raw.cast_const(), address);
    assert_eq!(drops.get(), 0);
    // SAFETY: raw is the sole live owner from into_raw; no references remain.
    let recovered = unsafe { KBox::from_raw(raw) };
    assert_eq!(recovered.0.get(), 0);
    drop(recovered);
    assert_eq!(drops.get(), 1);
}

#[test]
fn raw_dst_roundtrip_retains_slice_and_trait_metadata() {
    let drops = Cell::new(0);
    let owned: KBox<[RawDropProbe<'_>]> =
        KBox::new_nowait([RawDropProbe(&drops), RawDropProbe(&drops)]).unwrap();
    let raw = KBox::into_raw(owned);
    assert_eq!(raw.len(), 2);
    // SAFETY: the original slice pointer and metadata retain sole ownership.
    drop(unsafe { KBox::from_raw(raw) });
    assert_eq!(drops.get(), 2);

    trait Value {
        fn value(&self) -> usize;
    }
    impl Value for RawDropProbe<'_> {
        fn value(&self) -> usize {
            self.0.get()
        }
    }
    let owned: KBox<dyn Value + '_> = KBox::new_nowait(RawDropProbe(&drops)).unwrap();
    let raw = KBox::into_raw(owned);
    // SAFETY: raw retains the original data pointer, vtable and sole ownership.
    let recovered = unsafe { KBox::from_raw(raw) };
    assert_eq!(recovered.value(), 2);
    drop(recovered);
    assert_eq!(drops.get(), 3);
}

#[test]
fn raw_zst_and_empty_slice_preserve_alignment() {
    #[repr(align(256))]
    struct Aligned;
    let raw = KBox::into_raw(KBox::new_nowait(Aligned).unwrap());
    assert!(!raw.is_null());
    assert_eq!(raw.addr() % 256, 0);
    // SAFETY: raw is the untouched pointer from the consumed ZST owner.
    drop(unsafe { KBox::from_raw(raw) });
    let owned: KBox<[Aligned]> = KBox::new_nowait([]).unwrap();
    let raw = KBox::into_raw(owned);
    assert_eq!(raw.len(), 0);
    assert_eq!(raw.cast::<Aligned>().addr() % 256, 0);
    // SAFETY: the empty slice retains its original aligned pointer and metadata.
    drop(unsafe { KBox::from_raw(raw) });
}
