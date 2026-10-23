// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Drive the actual callback/cancellation functions after explicitly ending
//! payload borrows. This checks ownership, not the RCU engine's GP detection.

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
fn raw_box_borrow_survives_record_preparation_until_release() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = Box::into_raw(Box::new(Probe(drops.clone())));
    // SAFETY: retain the sole raw Box ownership through the last borrow use.
    let borrowed = unsafe { &*owner };
    let mut record = Retirement::<Box<Probe>>::prepare();
    record.owner = owner.cast();
    let raw = Box::into_raw(record);
    // SAFETY: raw projection from the record allocation retains its provenance.
    let node = unsafe { &raw mut (*raw).callback };
    assert_eq!(borrowed.0.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&drops), 2);
    // SAFETY: this isolated callback has never been queued and this is its only
    // invocation. All payload borrows ended above, simulating GP eligibility.
    unsafe { release::<Box<Probe>>(node) };
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[test]
fn cancellation_does_not_reclaim_a_still_borrowed_payload() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = Box::into_raw(Box::new(Probe(drops.clone())));
    // SAFETY: fixture retains raw ownership for explicit cleanup after borrow.
    let borrowed = unsafe { &*owner };
    let mut record = Retirement::<Box<Probe>>::prepare();
    record.owner = owner.cast();
    let raw = Box::into_raw(record);
    // SAFETY: field projection preserves provenance of the entire record Box.
    let node = unsafe { &raw mut (*raw).callback };
    // SAFETY: unique unqueued record; direct cancellation releases the record,
    // but must not consume its still-borrowed protected payload. The kernel
    // tests exercise the separate PreparedCallback submission integration.
    unsafe { cancel::<Box<Probe>>(node) };
    assert_eq!(borrowed.0.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&drops), 2);
    // SAFETY: cancellation intentionally retained the raw owner. This fixture
    // has now ended every borrower and reclaims it once to avoid a test leak.
    drop(unsafe { Box::from_raw(owner) });
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[test]
fn callback_releases_only_the_retired_arc_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let retained = Arc::new(Probe(drops.clone()));
    let mut record = Retirement::<Arc<Probe>>::prepare();
    record.owner = Arc::into_raw(retained.clone()).cast_mut().cast();
    let raw = Box::into_raw(record);
    // SAFETY: unique stable record, with an independently owned Arc reference.
    let node = unsafe { &raw mut (*raw).callback };
    // SAFETY: no payload borrower and no queue handle exists in this fixture;
    // this sole callback may consume precisely its recorded strong reference.
    unsafe { release::<Arc<Probe>>(node) };
    assert_eq!(Arc::strong_count(&retained), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(retained);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
