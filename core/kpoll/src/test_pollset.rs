use core::task::{RawWaker, RawWakerVTable, Waker};

use unittest::{assert_eq, def_test};

use crate::{PollSet, PollSetGroup};

// Helper to create a no-op waker for testing
fn noop_waker() -> Waker {
    fn raw_clone(_: *const ()) -> RawWaker {
        noop_raw_waker()
    }
    fn raw_wake(_: *const ()) {}
    fn raw_wake_by_ref(_: *const ()) {}
    fn raw_drop(_: *const ()) {}

    fn noop_raw_waker() -> RawWaker {
        RawWaker::new(
            core::ptr::null(),
            &RawWakerVTable::new(raw_clone, raw_wake, raw_wake_by_ref, raw_drop),
        )
    }

    unsafe { Waker::from_raw(noop_raw_waker()) }
}

#[def_test]
fn test_pollset_basic_register_and_wake() {
    // Test basic waker registration and wake
    let poll_set = PollSet::new();
    let waker = noop_waker();

    // Register waker
    poll_set.register(&waker);

    // Wake should return 1 (one waker woken)
    let count = poll_set.wake();
    assert_eq!(count, 1);
}

#[def_test]
fn test_pollset_multiple_wakers() {
    // Test registering multiple wakers
    let poll_set = PollSet::new();

    for _ in 0..10 {
        let waker = noop_waker();
        poll_set.register(&waker);
    }

    // Wake should return 10 (all wakers woken)
    let count = poll_set.wake();
    assert_eq!(count, 10);
}

#[def_test]
fn test_pollset_capacity_overflow() {
    // Test behavior when exceeding capacity (64 wakers)
    let poll_set = PollSet::new();

    // Register more than capacity
    for _ in 0..100 {
        let waker = noop_waker();
        poll_set.register(&waker);
    }

    // Wake should return 64 (capacity limit)
    let count = poll_set.wake();
    assert_eq!(count, 64);
}

#[def_test]
fn test_pollset_group_wake_all() {
    // Test PollSetGroup wake_all
    let mut group = PollSetGroup::new();

    let total = group.wake_all();
    assert_eq!(total, 9);
}
