// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[def_test]
fn borrowed_views_and_debug_forward_to_a_slice() {
    extern crate alloc;

    let mut owned: KBox<[u32]> = KBox::new_nowait([2, 4, 6]).unwrap();
    let address = owned.as_ptr();
    assert_eq!(AsRef::<[u32]>::as_ref(&owned).as_ptr(), address);
    AsMut::<[u32]>::as_mut(&mut owned)[1] = 9;
    assert_eq!(&*owned, &[2, 9, 6]);
    assert_eq!(alloc::format!("{owned:?}"), "[2, 9, 6]");
}

use core::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

use unittest::def_test;

use super::*;

#[def_test]
fn raw_owner_roundtrip_preserves_address_and_drops_once() {
    let drops = Cell::new(0);
    let owned = KBox::new_nowait(DropProbe(&drops)).unwrap();
    let address = &*owned as *const DropProbe<'_>;
    let raw = KBox::into_raw(owned);
    assert_eq!(raw.cast_const(), address);
    assert_eq!(drops.get(), 0);
    // SAFETY: raw is the sole live owner from into_raw; no references remain.
    let recovered = unsafe { KBox::from_raw(raw) };
    assert_eq!(recovered.0.get(), 0);
    drop(recovered);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn leak_preserves_address_metadata_and_skips_drop() {
    let drops = Cell::new(0);
    let owned: KBox<[DropProbe<'_>]> =
        KBox::new_nowait([DropProbe(&drops), DropProbe(&drops)]).unwrap();
    let address = owned.as_ptr();
    let leaked = KBox::leak(owned);
    assert_eq!(leaked.as_ptr(), address);
    assert_eq!(leaked.len(), 2);
    assert_eq!(drops.get(), 0);
    leaked[0].0.set(7);
    let shared: &[DropProbe<'_>] = leaked;
    assert_eq!(shared[0].0.get(), 7);
}

#[def_test]
fn raw_dst_roundtrip_retains_slice_and_trait_metadata() {
    let drops = Cell::new(0);
    let owned: KBox<[DropProbe<'_>]> =
        KBox::new_nowait([DropProbe(&drops), DropProbe(&drops)]).unwrap();
    let raw = KBox::into_raw(owned);
    assert_eq!(raw.len(), 2);
    // SAFETY: the original slice pointer and metadata retain sole ownership.
    drop(unsafe { KBox::from_raw(raw) });
    assert_eq!(drops.get(), 2);

    trait Value {
        fn value(&self) -> usize;
    }
    impl Value for DropProbe<'_> {
        fn value(&self) -> usize {
            self.0.get()
        }
    }
    let owned: KBox<dyn Value + '_> = KBox::new_nowait(DropProbe(&drops)).unwrap();
    let raw = KBox::into_raw(owned);
    // SAFETY: raw retains the original data pointer, vtable and sole ownership.
    let recovered = unsafe { KBox::from_raw(raw) };
    assert_eq!(recovered.value(), 2);
    drop(recovered);
    assert_eq!(drops.get(), 3);
}

#[def_test]
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

#[def_test]
fn copied_slice_preserves_source_and_alignment() {
    #[repr(align(256))]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Aligned(u32);
    let source = [Aligned(3), Aligned(7)];
    let mut owned = KBox::from_slice_with_flags(&source, AllocFlags::NOWAIT).unwrap();
    assert_eq!(&*owned, &source);
    assert_eq!(owned.as_ptr().addr() % 256, 0);
    owned[0].0 = 9;
    assert_eq!(source[0].0, 3);
    assert!(KBox::<[u8]>::from_slice(&[]).unwrap().is_empty());
    let zsts = KBox::<[()]>::new_slice((), usize::MAX).unwrap();
    let copied = KBox::from_slice(&zsts).unwrap();
    assert_eq!(copied.len(), usize::MAX);
    assert_eq!(
        KBox::<[u8]>::from_slice_with_flags(&[], AllocFlags::from_bits_retain(u32::MAX)).err(),
        Some(KError::InvalidInput),
    );
}

#[def_test]
fn runtime_slice_layout_and_element_lifecycle() {
    #[repr(align(256))]
    struct Aligned<'a>(DropProbe<'a>);
    let drops = Cell::new(0);
    let mut storage =
        KBox::<[Aligned<'_>]>::new_uninit_slice_with_flags(3, AllocFlags::NOWAIT).unwrap();
    let address = storage.as_ptr().cast::<()>();
    assert_eq!(address.addr() % 256, 0);
    for slot in storage.iter_mut() {
        slot.write(Aligned(DropProbe(&drops)));
    }
    // SAFETY: all three elements were initialized by the loop above.
    let owned = unsafe { storage.assume_init() };
    assert_eq!(owned.len(), 3);
    assert_eq!(owned.as_ptr().cast::<()>(), address);
    assert_eq!(owned[0].0.0.get(), 0);
    drop(owned);
    assert_eq!(drops.get(), 3);
}

#[def_test]
fn runtime_slice_rejects_invalid_layout_before_allocator() {
    let called = Cell::new(false);
    let result =
        KBox::<[u64]>::reserve_slice_with_allocator(usize::MAX, AllocFlags::NOWAIT, |_, _| {
            called.set(true);
            Err(KError::NoMemory)
        });
    assert!(matches!(result, Err(KError::InvalidInput)));
    assert!(!called.get());
    let flags = AllocFlags::from_bits_retain(u32::MAX);
    assert!(matches!(
        KBox::<[()]>::new_uninit_slice_with_flags(0, flags),
        Err(KError::InvalidInput)
    ));
    let result =
        KBox::<[u32]>::reserve_slice_with_allocator(5, AllocFlags::NOWAIT, |layout, flags| {
            core::assert_eq!(layout, Layout::array::<u32>(5).unwrap());
            core::assert_eq!(flags, AllocFlags::NOWAIT);
            Err(KError::NoMemory)
        });
    assert!(matches!(result, Err(KError::NoMemory)));
}

#[def_test]
fn runtime_slice_empty_zst_and_copied_contents() {
    #[repr(align(256))]
    struct AlignedZero;
    let owned =
        KBox::<[AlignedZero]>::reserve_slice_with_allocator(7, AllocFlags::NOWAIT, |_, _| {
            Err(KError::NoMemory)
        })
        .unwrap();
    assert_eq!(owned.len(), 7);
    assert_eq!(owned.as_ptr().addr() % 256, 0);
    let empty = KBox::<[u64]>::new_uninit_slice_with_flags(0, AllocFlags::NOWAIT).unwrap();
    assert_eq!(empty.len(), 0);
    let values = KBox::new_slice_with_flags(19u32, 4, AllocFlags::NOWAIT).unwrap();
    assert_eq!(&*values, &[19, 19, 19, 19]);
    let reservation =
        KBox::<[DropProbe<'_>]>::new_uninit_slice_with_flags(3, AllocFlags::NOWAIT).unwrap();
    drop(reservation);
}

struct DropProbe<'a>(&'a Cell<usize>);

#[def_test]
fn copied_zst_slice_accepts_large_lengths_without_element_work() {
    #[repr(align(256))]
    #[derive(Clone, Copy)]
    struct AlignedZero;
    let owned = KBox::new_slice_with_flags(AlignedZero, usize::MAX, AllocFlags::NOWAIT).unwrap();
    assert_eq!(owned.len(), usize::MAX);
    assert_eq!(owned.as_ptr().addr() % 256, 0);
    drop(owned);
    let flags = AllocFlags::from_bits_retain(u32::MAX);
    assert!(matches!(
        KBox::new_slice_with_flags(AlignedZero, usize::MAX, flags),
        Err(KError::InvalidInput)
    ));
}

#[def_test]
fn initialized_zst_slice_runs_each_destructor() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[repr(align(256))]
    struct AlignedZero;
    impl Drop for AlignedZero {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let before = DROPS.load(Ordering::Relaxed);
    let mut storage =
        KBox::<[AlignedZero]>::new_uninit_slice_with_flags(3, AllocFlags::NOWAIT).unwrap();
    for slot in storage.iter_mut() {
        slot.write(AlignedZero);
    }
    // SAFETY: all three elements were initialized above.
    let owned = unsafe { storage.assume_init() };
    drop(owned);
    assert_eq!(DROPS.load(Ordering::Relaxed), before + 3);
}

#[def_test]
fn into_inner_transfers_noncopy_value_without_running_drop() {
    let drops = Cell::new(0);
    let owned = KBox::new_nowait(DropProbe(&drops)).unwrap();
    let value = KBox::into_inner(owned);
    assert_eq!(drops.get(), 0);
    assert!(core::ptr::eq(value.0, &drops));
    drop(value);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn into_inner_preserves_aligned_value_and_owned_array() {
    #[repr(align(256))]
    struct Aligned<'a> {
        value: usize,
        probes: [DropProbe<'a>; 3],
    }
    let drops = Cell::new(0);
    let mut owned = KBox::new_nowait(Aligned {
        value: 7,
        probes: core::array::from_fn(|_| DropProbe(&drops)),
    })
    .unwrap();
    owned.value = 19;
    let value = KBox::into_inner(owned);
    assert_eq!(value.value, 19);
    assert_eq!(value.probes.len(), 3);
    assert_eq!(core::ptr::from_ref(&value).addr() % 256, 0);
    assert_eq!(drops.get(), 0);
    drop(value);
    assert_eq!(drops.get(), 3);
}

#[def_test]
fn into_inner_preserves_aligned_zero_sized_destructor() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[repr(align(256))]
    struct AlignedZero;
    impl Drop for AlignedZero {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let before = DROPS.load(Ordering::Relaxed);
    let owned = KBox::new_with_allocator(AlignedZero, AllocFlags::NOWAIT, |_, _| {
        Err(KError::NoMemory)
    })
    .unwrap();
    let value = KBox::into_inner(owned);
    assert_eq!(DROPS.load(Ordering::Relaxed), before);
    drop(value);
    assert_eq!(DROPS.load(Ordering::Relaxed), before + 1);
}

#[def_test]
fn into_inner_can_consume_a_sized_fn_once_capture() {
    let drops = Cell::new(0);
    let probe = DropProbe(&drops);
    let callback = KBox::new_nowait(move || probe).unwrap();
    let callback = KBox::into_inner(callback);
    assert_eq!(drops.get(), 0);
    let captured = callback();
    assert_eq!(drops.get(), 0);
    drop(captured);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn shared_callback_forwards_arguments_and_preserves_captures() {
    fn invoke(callback: impl Fn(usize, usize) -> usize) -> usize {
        callback(2, 3)
    }
    let calls = Cell::new(0);
    let drops = Cell::new(0);
    let probe = DropProbe(&drops);
    let callback = KBox::new(|left, right| {
        calls.set(calls.get() + 1);
        left + right + probe.drops()
    })
    .unwrap();
    let callback: KBox<dyn Fn(usize, usize) -> usize + '_> = callback;
    assert_eq!(invoke(&callback), 5);
    assert_eq!(invoke(callback), 5);
    assert_eq!(calls.get(), 2);
    assert_eq!(drops.get(), 0);
    drop(probe);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn mutable_callback_consumption_drops_captures_once() {
    fn consume(callback: impl FnOnce(usize) -> usize) -> usize {
        callback(3)
    }
    let drops = Cell::new(0);
    let probe = DropProbe(&drops);
    let mut total = 0;
    let callback = KBox::new(move |increment| {
        total += increment;
        total + probe.drops()
    })
    .unwrap();
    let mut callback: KBox<dyn FnMut(usize) -> usize + '_> = callback;
    assert_eq!(callback(2), 2);
    assert_eq!(consume(callback), 5);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn callback_allocation_failure_drops_capture_without_calling_it() {
    for flags in [AllocFlags::KERNEL, AllocFlags::NOWAIT] {
        let calls = Cell::new(0);
        let calls_ref = &calls;
        let drops = Cell::new(0);
        let probe = DropProbe(&drops);
        let callback = move || {
            calls_ref.set(calls_ref.get() + 1);
            probe.drops()
        };
        let result = KBox::new_with_allocator(callback, flags, |_, requested| {
            assert_eq!(requested, flags);
            Err(KError::NoMemory)
        });
        assert_eq!(result.err(), Some(KError::NoMemory));
        assert_eq!(calls.get(), 0);
        assert_eq!(drops.get(), 1);
    }
}

#[def_test]
fn boxed_iterators_forward_and_support_owned_adapters() {
    let values = [2, 3, 5, 7];
    let mut sized = KBox::new(values.iter()).unwrap();
    assert_eq!(sized.next(), Some(&2));
    assert_eq!(sized.size_hint(), (3, Some(3)));
    let mut erased: KBox<dyn Iterator<Item = &i32> + '_> = sized;
    assert_eq!(erased.nth(1), Some(&5));
    assert_eq!(erased.size_hint(), (1, Some(1)));
    let mut chained = erased.chain([&11]).enumerate().skip(1);
    assert_eq!(chained.next(), Some((1, &11)));
    assert_eq!(chained.next(), None);
}

#[def_test]
fn failed_outer_iterator_allocation_drops_both_owners() {
    let drops = Cell::new(0);
    let first: KBox<dyn Iterator<Item = DropProbe<'_>> + '_> =
        KBox::new(core::iter::once(DropProbe(&drops))).unwrap();
    let second: KBox<dyn Iterator<Item = DropProbe<'_>> + '_> =
        KBox::new(core::iter::once(DropProbe(&drops))).unwrap();
    let result = KBox::new_with_allocator(first.chain(second), AllocFlags::KERNEL, |_, _| {
        Err(KError::NoMemory)
    });
    assert_eq!(result.err(), Some(KError::NoMemory));
    assert_eq!(drops.get(), 2);
}

trait Probe {
    fn drops(&self) -> usize;
}

impl Probe for DropProbe<'_> {
    fn drops(&self) -> usize {
        self.0.get()
    }
}

#[def_test]
fn trait_object_preserves_address_and_destroys_concrete_value() {
    let drops = Cell::new(0);
    let owned = KBox::new_nowait(DropProbe(&drops)).unwrap();
    let address = core::ptr::from_ref(&*owned).addr();
    let erased: KBox<dyn Probe + '_> = owned;
    assert_eq!(core::ptr::from_ref(&*erased).addr(), address);
    assert_eq!(erased.drops(), 0);
    assert_eq!(Layout::for_value(&*erased), Layout::new::<DropProbe<'_>>());
    drop(erased);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn slice_coercion_preserves_length_and_drops_every_element() {
    let drops = Cell::new(0);
    let owned =
        KBox::new_nowait([DropProbe(&drops), DropProbe(&drops), DropProbe(&drops)]).unwrap();
    let mut slice: KBox<[DropProbe<'_>]> = owned;
    assert_eq!(slice.len(), 3);
    slice.swap(0, 2);
    assert_eq!(
        Layout::for_value(&*slice),
        Layout::new::<[DropProbe<'_>; 3]>()
    );
    drop(slice);
    assert_eq!(drops.get(), 3);
    let empty: KBox<[u64]> = KBox::new_nowait([]).unwrap();
    assert!(empty.is_empty());
}

#[def_test]
fn aligned_zero_sized_trait_object_skips_storage_and_runs_drop() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[repr(align(4096))]
    struct AlignedZero;
    impl Drop for AlignedZero {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let before = DROPS.load(Ordering::Relaxed);
    let owned = KBox::new_with_allocator(AlignedZero, AllocFlags::NOWAIT, |_, _| {
        Err(KError::NoMemory)
    })
    .unwrap();
    let erased: KBox<dyn core::any::Any + Send + Sync> = owned;
    assert_eq!(Layout::for_value(&*erased), Layout::new::<AlignedZero>());
    assert_eq!(core::ptr::from_ref(&*erased).addr() % 4096, 0);
    assert!(erased.is::<AlignedZero>());
    drop(erased);
    assert_eq!(DROPS.load(Ordering::Relaxed), before + 1);
}

#[def_test]
fn unsized_owner_retains_pointer_layout_and_thread_bounds() {
    fn assert_send_sync<Value: Send + Sync>() {}
    assert_send_sync::<KBox<dyn core::any::Any + Send + Sync>>();
    assert_eq!(
        core::mem::size_of::<KBox<u64>>(),
        core::mem::size_of::<*mut u64>()
    );
    assert_eq!(
        core::mem::size_of::<KBox<[u64]>>(),
        core::mem::size_of::<*mut [u64]>()
    );
}

impl Drop for DropProbe<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

#[def_test]
fn failed_allocation_drops_input_once() {
    let drops = Cell::new(0);
    for flags in [
        AllocFlags::NOWAIT,
        AllocFlags::KERNEL,
        AllocFlags::NOIO,
        AllocFlags::NOFS,
        AllocFlags::KERNEL & !AllocFlags::DIRECT_RECLAIM,
    ] {
        let result = KBox::new_with_allocator(DropProbe(&drops), flags, |layout, requested| {
            assert_eq!(layout, Layout::new::<DropProbe<'_>>());
            assert_eq!(requested, flags);
            Err(KError::NoMemory)
        });
        assert_eq!(result.err(), Some(KError::NoMemory));
    }
    assert_eq!(drops.get(), 5);
}

#[def_test]
fn owned_value_is_dropped_once() {
    let drops = Cell::new(0);
    let owned = KBox::new_nowait(DropProbe(&drops)).unwrap();
    assert_eq!(drops.get(), 0);
    drop(owned);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn zero_sized_value_skips_allocator_and_runs_drop() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    #[repr(align(4096))]
    struct AlignedZero;
    impl Drop for AlignedZero {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let before = DROPS.load(Ordering::Relaxed);
    let owned = KBox::new_with_allocator(AlignedZero, AllocFlags::NOWAIT, |_, _| {
        Err(KError::NoMemory)
    })
    .unwrap();
    assert_eq!(core::ptr::from_ref(&*owned).addr() % 4096, 0);
    drop(owned);
    assert_eq!(DROPS.load(Ordering::Relaxed), before + 1);
}

#[def_test]
fn aligned_value_remains_stable_across_owner_move() {
    #[repr(align(4096))]
    struct AlignedValue(u64);
    for flags in [
        AllocFlags::NOWAIT,
        AllocFlags::KERNEL,
        AllocFlags::NOIO,
        AllocFlags::NOFS,
    ] {
        let mut owned = KBox::new_with_flags(AlignedValue(7), flags).unwrap();
        let address = core::ptr::from_ref(&*owned).addr();
        assert_eq!(address % 4096, 0);
        owned.0 = 11;
        let moved = owned;
        assert_eq!(core::ptr::from_ref(&*moved).addr(), address);
        assert_eq!(moved.0, 11);
    }
}

#[def_test]
fn owner_preserves_send_sync_bounds() {
    fn assert_send<Value: Send>() {}
    fn assert_sync<Value: Sync>() {}
    assert_send::<KBox<Cell<u64>>>();
    assert_sync::<KBox<u64>>();
}

#[def_test]
fn default_constructor_uses_kernel_policy() {
    assert_eq!(AllocFlags::default(), AllocFlags::KERNEL);
    let owned: KResult<KBox<u64>> = KBox::new(17);
    assert_eq!(*owned.unwrap(), 17);
    assert!(!AllocFlags::NOIO.intersects(AllocFlags::IO | AllocFlags::FS));
    assert!(!AllocFlags::NOFS.contains(AllocFlags::FS));
    assert!(!AllocFlags::NOWAIT.contains(AllocFlags::DIRECT_RECLAIM));
}

#[def_test]
fn unknown_flags_fail_before_allocation_and_drop_input() {
    let drops = Cell::new(0);
    let was_called = Cell::new(false);
    let flags = AllocFlags::from_bits_retain(1 << 31);
    let result = KBox::new_with_allocator(DropProbe(&drops), flags, |_, _| {
        was_called.set(true);
        Err(KError::NoMemory)
    });
    assert_eq!(result.err(), Some(KError::InvalidInput));
    assert_eq!(drops.get(), 1);
    assert!(!was_called.get());
    assert_eq!(
        KBox::new_with_flags((), flags).err(),
        Some(KError::InvalidInput)
    );
}

#[def_test]
fn reserved_storage_initializes_in_place_and_drops_once() {
    #[repr(align(256))]
    struct Aligned<'a>(DropProbe<'a>);
    let drops = Cell::new(0);
    let storage = KBox::<Aligned<'_>>::new_uninit().unwrap();
    let address = core::ptr::from_ref(&*storage).cast::<()>();
    assert_eq!(address.addr() % 256, 0);
    let owned = KBox::write(storage, Aligned(DropProbe(&drops)));
    assert_eq!(core::ptr::from_ref(&*owned).cast::<()>(), address);
    assert_eq!(owned.0.0.get(), 0);
    drop(owned);
    assert_eq!(drops.get(), 1);
}

#[def_test]
fn abandoned_reservation_does_not_drop_uninitialized_value() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    struct Probe;
    impl Drop for Probe {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let before = DROPS.load(Ordering::Relaxed);
    drop(KBox::<Probe>::new_uninit_with_flags(AllocFlags::NOWAIT).unwrap());
    drop(KBox::<(Probe, usize)>::new_uninit_with_flags(AllocFlags::NOWAIT).unwrap());
    assert_eq!(DROPS.load(Ordering::Relaxed), before);
    drop(KBox::write(KBox::<Probe>::new_uninit().unwrap(), Probe));
    assert_eq!(DROPS.load(Ordering::Relaxed), before + 1);
}

#[def_test]
fn reservation_rejects_invalid_flags_and_propagates_allocation_failure() {
    let flags = AllocFlags::from_bits_retain(u32::MAX);
    assert!(matches!(
        KBox::<()>::new_uninit_with_flags(flags),
        Err(KError::InvalidInput)
    ));
    let result = KBox::new_with_allocator(
        MaybeUninit::<usize>::uninit(),
        AllocFlags::KERNEL,
        |layout, flags| {
            assert_eq!(layout, Layout::new::<usize>());
            assert_eq!(flags, AllocFlags::KERNEL);
            Err(KError::NoMemory)
        },
    );
    assert!(matches!(result, Err(KError::NoMemory)));
}
