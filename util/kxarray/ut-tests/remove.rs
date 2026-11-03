// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kalloc_types::{AllocFlags, KBox};
use unittest::{assert, assert_eq, def_test};

use crate::XArray;

struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn removal_reuses_keys_and_releases_each_owner_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let array = XArray::<KBox<DropProbe>>::new();
    let keys = [0, 63, 64, 4096, usize::MAX];
    for key in keys {
        assert_eq!(
            // SAFETY: watched sleepable task outside scopes/locks; permanent service.
            unsafe { array.remove_with_flags(key, AllocFlags::NOWAIT) },
            Ok(false)
        );
        for _ in 0..2 {
            assert!(
                // SAFETY: task context, atomic-only destructor, nonblocking KBox/Arc
                // deallocation and independently owned callback resources.
                unsafe {
                    array.insert_with_flags(
                        key,
                        KBox::new_nowait(DropProbe(drops.clone())).unwrap(),
                        AllocFlags::NOWAIT,
                    )
                }
                .is_ok()
            );
            assert_eq!(
                // SAFETY: same watched task, no reader or bookkeeping lock held.
                unsafe { array.remove_with_flags(key, AllocFlags::NOWAIT) },
                Ok(true)
            );
            assert!(array.root.load(Ordering::Acquire).is_null());
            assert!(krcu::with_read(|scope| array.read(scope, key).is_none()));
            assert_eq!(
                // SAFETY: same valid submission context; missing entries own nothing.
                unsafe { array.remove_with_flags(key, AllocFlags::NOWAIT) },
                Ok(false)
            );
        }
    }
    drop(array);
    // SAFETY: sleepable watched task, callbacks do not wait on this test.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(drops.load(Ordering::Relaxed), keys.len() * 2);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[def_test]
fn pruning_preserves_siblings_and_does_not_compact_nonempty_root() {
    let array = XArray::<KBox<usize>>::new();
    for key in [0, 1, 64, usize::MAX] {
        assert!(
            // SAFETY: primitive release in an allocation-permitted task.
            unsafe {
                array.insert_with_flags(key, KBox::new_nowait(key).unwrap(), AllocFlags::NOWAIT)
            }
            .is_ok()
        );
    }
    let root = array.root.load(Ordering::Acquire);
    for key in [usize::MAX, 64, 1] {
        assert_eq!(
            // SAFETY: watched sleepable task, no scope/lock held, permanent service.
            unsafe { array.remove_with_flags(key, AllocFlags::NOWAIT) },
            Ok(true)
        );
        assert_eq!(array.root.load(Ordering::Acquire), root);
        assert_eq!(
            krcu::with_read(|scope| array.read(scope, 0).copied()),
            Some(0)
        );
    }
    assert_eq!(
        // SAFETY: same admitted context, last entry owns only a primitive KBox.
        unsafe { array.remove_with_flags(0, AllocFlags::NOWAIT) },
        Ok(true)
    );
    assert!(array.root.load(Ordering::Acquire).is_null());
    // SAFETY: no callbacks depend on this test, no exclusion held.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
}

#[def_test]
fn rejected_removal_preserves_association() {
    let array = XArray::<KBox<usize>>::new();
    assert!(
        // SAFETY: initialized task and primitive payload with nonblocking release.
        unsafe { array.insert_with_flags(0, KBox::new_nowait(7).unwrap(), AllocFlags::NOWAIT,) }
            .is_ok()
    );
    let result = {
        let _irq = kspin::IrqSave::new();
        // SAFETY: task holds no bookkeeping lock; preflight must reject IRQ
        // masking before withdrawing ownership or allocating a retirement record.
        unsafe { array.remove_with_flags(0, AllocFlags::NOWAIT) }
    };
    assert_eq!(result, Err(kerrno::KError::InvalidInput));
    assert_eq!(
        krcu::with_read(|scope| array.read(scope, 0).copied()),
        Some(7)
    );
}

#[def_test]
fn removal_releases_only_the_arrays_arc_reference() {
    let old = Arc::new(7_usize);
    let array = XArray::<Arc<usize>>::new();
    assert!(
        // SAFETY: ordinary watched task, primitive payload and nonblocking release.
        unsafe { array.insert_with_flags(usize::MAX, old.clone(), AllocFlags::NOWAIT) }.is_ok()
    );
    let retained = krcu::with_read(|scope| array.get_owned(scope, usize::MAX)).unwrap();
    assert_eq!(
        // SAFETY: watched task outside scopes/locks with permanent RCU service.
        unsafe { array.remove_with_flags(usize::MAX, AllocFlags::NOWAIT) },
        Ok(true)
    );
    drop(array);
    // SAFETY: callbacks own only independent references and do not await us.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(Arc::strong_count(&old), 2);
    assert_eq!(*retained, 7);
}
