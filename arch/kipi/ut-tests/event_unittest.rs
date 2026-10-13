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
    });
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
