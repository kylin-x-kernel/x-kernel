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
    let raw = Box::into_raw(Box::new(Node::new(Probe(drops.clone()))));
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
    let raw = Box::into_raw(Box::new(Node::new(Probe(drops.clone()))));
    // SAFETY: project the embedded record from its original Box pointer.
    let callback = unsafe { &raw mut (*raw).callback };
    // SAFETY: live unique unqueued record; cancellation must retain it.
    unsafe { retain_rejected(callback) };
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    // SAFETY: test holds the retained ownership, with no publication/readers.
    unsafe { release::<Probe>(callback) };
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
