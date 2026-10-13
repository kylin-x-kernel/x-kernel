// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use std::sync::Arc;

struct Counter;

impl Counter {
    fn ping(&self) -> bool {
        true
    }
}

lazy_static! {
    /// A lazily initialized `Arc<Counter>`.
    static ref DRV: Arc<Counter> = Arc::new(Counter);
    static ref VALUE: u32 = 42;
}

/// `STATIC.clone()` must resolve to `T::clone` via autoderef (the upstream
/// `lazy_static` dispatch contract), not clone the wrapper.
#[test]
fn clone_returns_inner_type() {
    let cloned: Arc<Counter> = DRV.clone();
    assert!(Arc::ptr_eq(&cloned, &*DRV));
}

/// Methods on the inner type resolve through `Deref`.
#[test]
fn inner_methods_dispatch() {
    assert!(DRV.ping());
}

/// `Deref` reaches the underlying value and triggers one-time init.
#[test]
fn deref_initializes_and_reads() {
    assert_eq!(*VALUE, 42);
}

/// A `static ref` may be declared `pub`; the inner value is reachable
/// from sibling code paths that only see the static.
#[test]
fn pub_static_is_usable() {
    let v: &u32 = &*VALUE;
    assert_eq!(*v, 42);
}
