// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::def_test;

use super::{Callback, MulticastCallback};

#[def_test]
fn test_callback_executes() {
    static HIT: AtomicUsize = AtomicUsize::new(0);
    let cb = Callback::new(|| {
        HIT.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();
    cb.call();
    assert_eq!(HIT.load(Ordering::SeqCst), 1);
}

#[def_test]
fn test_multicast_clone_and_call() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let cb = MulticastCallback::new(|| {
        COUNT.fetch_add(1, Ordering::SeqCst);
    });
    cb.clone().call();
    cb.call();
    assert_eq!(COUNT.load(Ordering::SeqCst), 2);
}

#[def_test]
fn test_unicast_from_multicast() {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let mc = MulticastCallback::new(|| {
        COUNT.fetch_add(1, Ordering::SeqCst);
    });
    let uc = mc.into_unicast();
    uc.call();
    assert_eq!(COUNT.load(Ordering::SeqCst), 1);
}

struct DropProbe(alloc::sync::Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn one_shot_consumes_non_copy_capture_once() {
    for flags in [
        kalloc_types::AllocFlags::KERNEL,
        kalloc_types::AllocFlags::NOWAIT,
    ] {
        let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
        let probe = DropProbe(drops.clone());
        let callback = Callback::new_with_flags(move || drop(probe), flags).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        callback.call();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn cancellation_and_invalid_policy_drop_captures_without_calling() {
    let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
    let calls = alloc::sync::Arc::new(AtomicUsize::new(0));
    for flags in [
        kalloc_types::AllocFlags::NOWAIT,
        kalloc_types::AllocFlags::from_bits_retain(1 << 31),
    ] {
        let probe = DropProbe(drops.clone());
        let hits = calls.clone();
        let callback = Callback::new_with_flags(
            move || {
                hits.fetch_add(1, Ordering::Relaxed);
                drop(probe);
            },
            flags,
        );
        if flags == kalloc_types::AllocFlags::NOWAIT {
            assert!(callback.is_ok());
        } else {
            assert!(matches!(callback, Err(kerrno::KError::InvalidInput)));
        }
        drop(callback);
    }
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[def_test]
fn multicast_dispatch_reuses_shared_owner() {
    let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
    let probe = DropProbe(drops.clone());
    let callback = MulticastCallback::new(move || {
        let _capture = &probe;
    });
    let first = callback.clone().into_unicast();
    let second = callback.clone().into_unicast();
    let super::CallbackKind::Shared(ref shared) = first.0 else {
        panic!("multicast dispatch must retain shared ownership");
    };
    assert!(alloc::sync::Arc::ptr_eq(&callback.0, &shared.0));
    drop(callback);
    first.call();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    second.call();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
