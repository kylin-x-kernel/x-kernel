// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    cell::Cell,
    sync::atomic::{AtomicUsize, Ordering},
};

use unittest::def_test;

use super::*;

struct DropProbe<'a>(&'a Cell<usize>);

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
