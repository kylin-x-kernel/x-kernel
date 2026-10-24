// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::AtomicUsize;

use unittest::{assert, assert_eq, def_test};

use super::*;

struct Probe(Arc<AtomicUsize>);
impl Drop for Probe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn ordered_action_iteration_and_removal() {
    let list = RcuList::new();
    for action in 0..4 {
        let entry = Entry::new(action);
        // SAFETY: kernel UT runs in task context, with no reentrant mutation.
        unsafe { list.push_back(entry) };
    }
    assert_eq!(
        krcu::with_read(|scope| list.iter(scope).copied().collect::<Vec<_>>()),
        [0, 1, 2, 3]
    );
    assert_eq!(
        // SAFETY: watched sleepable task; usize/allocator release is nonblocking.
        unsafe { list.remove_first(|action| *action == 1) },
        Ok(true)
    );
    assert_eq!(
        // SAFETY: same context and release contract; predicate only compares values.
        unsafe { list.remove_first(|action| *action == 2) },
        Ok(true)
    );
    assert_eq!(
        krcu::with_read(|scope| list.iter(scope).copied().collect::<Vec<_>>()),
        [0, 3]
    );
    // SAFETY: no read scope or lock remains; no callback depends on this task.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
}

#[def_test]
fn callback_outlives_list_and_releases_only_list_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let action = Arc::new(Probe(drops.clone()));
    let list = RcuList::new();
    // SAFETY: initialized task context; entry allocation precedes writer locking.
    unsafe { list.push_back(Entry::new(action.clone())) };
    let observed = krcu::with_read(|scope| (list.iter(scope).count(), Arc::strong_count(&action)));
    assert_eq!(observed, (1, 2));
    // SAFETY: Probe only updates an atomic; Arc/Box deallocation is nonblocking.
    assert_eq!(unsafe { list.remove_first(|_| true) }, Ok(true));
    drop(list);
    // SAFETY: caller holds no read scope/lock; callbacks need no task progress.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert_eq!(Arc::strong_count(&action), 1);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(action);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn rejected_context_does_not_invoke_predicate_or_unlink() {
    let list = RcuList::new();
    // SAFETY: normal task context with no reentrant mutation.
    unsafe { list.push_back(Entry::new(3)) };
    let calls = AtomicUsize::new(0);
    let result = {
        let _irq = kspin::IrqSave::new();
        // SAFETY: nonblocking primitive release and predicate; the admission
        // check rejects IRQ masking before any ownership or topology change.
        unsafe {
            list.remove_first(|_| {
                calls.fetch_add(1, Ordering::Relaxed);
                true
            })
        }
    };
    assert_eq!(result, Err(kerrno::KError::InvalidInput));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        krcu::with_read(|scope| list.iter(scope).copied().collect::<Vec<_>>()),
        [3]
    );
}
