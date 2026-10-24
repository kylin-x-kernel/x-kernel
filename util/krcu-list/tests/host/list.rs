// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host fixtures retain every detached allocation until all borrows end.
//! They exercise real links but do not pretend to run a kernel grace period.

extern crate std;

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::AtomicUsize;

use super::*;

fn append<T: Send + Sync + 'static>(list: &RcuList<T>, value: T) {
    // SAFETY: host fixture uses normal threads and a real SMP spinlock; no IRQ
    // entry or reentrant list writer exists. Entry preparation is outside it.
    unsafe { list.push_back(Entry::new(value)) };
}

/// # Safety
/// The fixture must retain all reachable allocations until this borrow ends.
unsafe fn view<T: Send + Sync + 'static>(list: &RcuList<T>) -> Iter<'_, T> {
    Iter {
        next: list.head.load(Ordering::Acquire),
        lifetime: PhantomData,
    }
}

fn withdraw<T: Send + Sync + 'static>(
    list: &RcuList<T>,
    mut predicate_fn: impl FnMut(&T) -> bool,
) -> *mut Node<T> {
    list.withdraw(&mut predicate_fn)
}

/// # Safety
/// raw owns a detached node; the fixture has ended all old borrows.
unsafe fn reclaim<T>(raw: *mut Node<T>) {
    assert!(!raw.is_null());
    // SAFETY: fixture certifies unique allocation ownership and no readers.
    drop(unsafe { Box::from_raw(raw) });
}

#[test]
fn append_preserves_order_and_empty_end() {
    let list = RcuList::new();
    // SAFETY: empty list, no reclamation.
    assert_eq!(unsafe { view(&list) }.count(), 0);
    for value in 0..4 {
        append(&list, value);
    }
    assert_eq!(
        // SAFETY: all four allocations remain owned by list.
        unsafe { view(&list) }.copied().collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
}

#[test]
fn removed_current_and_successor_keep_old_path() {
    let list = RcuList::new();
    for value in 0..4 {
        append(&list, value);
    }
    // SAFETY: this fixture retains both withdrawn entries until the final use.
    let mut reader = unsafe { view(&list) };
    assert_eq!(reader.next(), Some(&0));
    let removed_b = withdraw(&list, |value| *value == 1);
    let removed_c = withdraw(&list, |value| *value == 2);
    assert_eq!(reader.copied().collect::<Vec<_>>(), [1, 2, 3]);
    // SAFETY: live list owns 0/3; old nodes are also retained.
    assert_eq!(unsafe { view(&list) }.copied().collect::<Vec<_>>(), [0, 3]);
    // SAFETY: all old iterator uses ended; each pointer owns one removed node.
    unsafe { reclaim(removed_b) };
    // SAFETY: no old iterator remains and this node has not been freed.
    unsafe { reclaim(removed_c) };
}

#[test]
fn removing_head_tail_and_last_repairs_append_position() {
    let list = RcuList::new();
    append(&list, 0);
    append(&list, 1);
    let head = withdraw(&list, |value| *value == 0);
    let tail = withdraw(&list, |value| *value == 1);
    assert!(list.head.load(Ordering::Relaxed).is_null());
    assert!(list.writers.lock().tail.is_null());
    append(&list, 2);
    // SAFETY: the new entry remains linked and no reclaim occurs.
    assert_eq!(unsafe { view(&list) }.copied().collect::<Vec<_>>(), [2]);
    // SAFETY: fixture has no readers of the two withdrawn entries.
    unsafe { reclaim(head) };
    // SAFETY: separate retired allocation, never reclaimed before.
    unsafe { reclaim(tail) };
}

#[test]
fn absent_match_does_not_change_ownership() {
    let list = RcuList::new();
    append(&list, 3);
    assert!(withdraw(&list, |value| *value == 4).is_null());
    // SAFETY: no node has been detached or freed.
    assert_eq!(unsafe { view(&list) }.copied().collect::<Vec<_>>(), [3]);
}

struct CountDrop(Arc<AtomicUsize>);
impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn prepared_live_and_retired_owners_release_once() {
    std::println!(
        "RCU_LIST_STORAGE usize_node_bytes={} arc_node_bytes={} entry_allocations=1 \
         retirement_allocations=0",
        core::mem::size_of::<Node<usize>>(),
        core::mem::size_of::<Node<Arc<usize>>>(),
    );
    let drops = Arc::new(AtomicUsize::new(0));
    drop(Entry::new(CountDrop(drops.clone())));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    let list = RcuList::new();
    append(&list, CountDrop(drops.clone()));
    append(&list, CountDrop(drops.clone()));
    let detached = withdraw(&list, |_| true);
    drop(list);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    // SAFETY: no list or reader remains; the detached allocation is independent.
    unsafe { reclaim(detached) };
    assert_eq!(drops.load(Ordering::Relaxed), 3);
}

#[test]
fn concurrent_writers_publish_each_entry_once() {
    let list = RcuList::new();
    std::thread::scope(|scope| {
        for writer in 0..4 {
            let list = &list;
            scope.spawn(move || {
                for item in 0..32 {
                    append(list, writer * 32 + item);
                }
            });
        }
    });
    // SAFETY: all writers joined and every allocation remains owned by list.
    let mut values = unsafe { view(&list) }.copied().collect::<Vec<_>>();
    values.sort_unstable();
    assert_eq!(values, (0..128).collect::<Vec<_>>());
}

#[test]
fn concurrent_unlink_preserves_borrowed_values_and_cursor() {
    let list = RcuList::new();
    for value in 0..4 {
        append(&list, value);
    }
    let phase = std::sync::Barrier::new(2);
    let mut removed = [ptr::null_mut(); 2];
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // SAFETY: coordinator retains every detached allocation until after
            // this thread joins. The fixture supplies lifetime, not a fake GP.
            let mut cursor = unsafe { view(&list) };
            let first = cursor.next();
            phase.wait();
            phase.wait();
            assert_eq!(first, Some(&0));
            assert_eq!(cursor.copied().collect::<Vec<_>>(), [1, 2, 3]);
        });
        phase.wait();
        removed[0] = withdraw(&list, |value| *value == 1);
        removed[1] = withdraw(&list, |value| *value == 2);
        phase.wait();
    });
    for raw in removed {
        // SAFETY: reader thread joined; each detached allocation is reclaimed once.
        unsafe { reclaim(raw) };
    }
}
