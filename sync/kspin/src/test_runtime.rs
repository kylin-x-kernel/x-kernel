#![cfg(unittest)]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{TestResult, assert, assert_eq, def_test};

use crate::{BaseGuard, SpinLock, SpinRaw};

struct TestGuard;

static GUARD_COUNTER: AtomicUsize = AtomicUsize::new(0);

impl BaseGuard for TestGuard {
    type State = usize;

    fn acquire() -> Self::State {
        GUARD_COUNTER.fetch_add(1, Ordering::SeqCst)
    }

    fn release(state: Self::State) {
        GUARD_COUNTER.store(state, Ordering::SeqCst);
    }
}

type TestSpinLock<T> = SpinLock<TestGuard, T>;


#[def_test]
fn test_spinlock_basic_lock_unlock() -> TestResult {
    let lock = SpinRaw::new(42);

    {
        let guard = lock.lock();
        assert_eq!(*guard, 42);
    }

    // After guard drops, should be able to lock again
    {
        let guard = lock.lock();
        assert_eq!(*guard, 42);
    }

    TestResult::Ok
}

#[def_test]
fn test_spinlock_mutable_access() -> TestResult {
    let lock = SpinRaw::new(0);

    {
        let mut guard = lock.lock();
        *guard = 100;
        assert_eq!(*guard, 100);
    }

    {
        let guard = lock.lock();
        assert_eq!(*guard, 100);
    }

    TestResult::Ok
}


#[def_test]
fn test_spinlock_zero_sized_type() -> TestResult {
    // Test with zero-sized type
    let lock = SpinRaw::new(());

    {
        let guard = lock.lock();
        assert_eq!(*guard, ());
    }

    TestResult::Ok
}

#[def_test]
fn test_spinlock_large_data_structure() -> TestResult {
    // Test with large data structure
    let large_vec: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
    let lock = SpinRaw::new(large_vec);

    {
        let mut guard = lock.lock();
        assert_eq!(guard.len(), 1000);
        assert_eq!(guard[0], 0);
        assert_eq!(guard[255], 255);
        assert_eq!(guard[256], 0); // Wraps around

        // Modify
        guard.push(42);
        assert_eq!(guard.len(), 1001);
    }

    {
        let guard = lock.lock();
        assert_eq!(guard.len(), 1001);
        assert_eq!(guard[1000], 42);
    }

    TestResult::Ok
}

#[def_test]
fn test_spinlock_boundary_values() -> TestResult {
    // Test with boundary numeric values
    let lock = SpinRaw::new(usize::MAX);

    {
        let mut guard = lock.lock();
        assert_eq!(*guard, usize::MAX);

        // Test wrapping
        *guard = guard.wrapping_add(1);
        assert_eq!(*guard, 0);
    }

    {
        let guard = lock.lock();
        assert_eq!(*guard, 0);
    }

    TestResult::Ok
}

#[def_test]
fn test_spinlock_nested_data_structures() -> TestResult {
    // Test with nested Vec
    let data = alloc::vec![
        alloc::vec![1, 2, 3],
        alloc::vec![4, 5, 6],
        alloc::vec![7, 8, 9]
    ];
    let lock = SpinRaw::new(data);

    {
        let mut guard = lock.lock();
        assert_eq!(guard.len(), 3);
        assert_eq!(guard[0][0], 1);
        assert_eq!(guard[2][2], 9);

        // Modify nested structure
        guard[1].push(99);
        guard.push(alloc::vec![10, 11]);
    }

    {
        let guard = lock.lock();
        assert_eq!(guard.len(), 4);
        assert_eq!(guard[1].len(), 4);
        assert_eq!(guard[1][3], 99);
        assert_eq!(guard[3][0], 10);
    }

    TestResult::Ok
}


#[def_test]
fn test_guard_acquire_release_tracking() -> TestResult {
    GUARD_COUNTER.store(0, Ordering::SeqCst);

    let lock = TestSpinLock::new(42);

    let initial = GUARD_COUNTER.load(Ordering::SeqCst);

    {
        let _guard = lock.lock();
        let during = GUARD_COUNTER.load(Ordering::SeqCst);
        assert_eq!(during, initial + 1);
    }

    // After guard drops, state should be restored
    let after = GUARD_COUNTER.load(Ordering::SeqCst);
    assert_eq!(after, initial);

    TestResult::Ok
}

#[def_test]
fn test_guard_multiple_acquisitions() -> TestResult {
    GUARD_COUNTER.store(0, Ordering::SeqCst);

    let lock = TestSpinLock::new(0);

    for i in 0..5 {
        {
            let mut guard = lock.lock();
            *guard = i;
            // Guard state is modified during lock
            assert!(GUARD_COUNTER.load(Ordering::SeqCst) > 0);
        }
        // After each release, state should be 0
        assert_eq!(GUARD_COUNTER.load(Ordering::SeqCst), 0);
    }

    {
        let guard = lock.lock();
        assert_eq!(*guard, 4);
    }

    TestResult::Ok
}

#[cfg(feature = "smp")]
#[def_test]
fn test_try_lock_success_when_available() -> TestResult {
    let lock = SpinRaw::new(42);

    // try_lock should succeed when unlocked
    {
        let guard = lock.try_lock();
        assert!(guard.is_some());
        if let Some(g) = guard {
            assert_eq!(*g, 42);
        }
    }

    // After guard drops, try_lock should succeed again
    {
        let guard = lock.try_lock();
        assert!(guard.is_some());
    }

    TestResult::Ok
}

#[cfg(feature = "smp")]
#[def_test]
fn test_try_lock_fails_when_locked() -> TestResult {
    let lock = SpinRaw::new(100);

    // Hold the lock
    let _guard1 = lock.lock();

    // try_lock should fail
    let guard2 = lock.try_lock();
    assert!(guard2.is_none());

    drop(_guard1);

    // After releasing, try_lock should succeed
    let guard3 = lock.try_lock();
    assert!(guard3.is_some());

    TestResult::Ok
}


#[def_test]
fn test_into_inner_extracts_value() -> TestResult {
    let lock = SpinRaw::new(alloc::vec![1, 2, 3, 4, 5]);

    let vec = lock.into_inner();
    assert_eq!(vec.len(), 5);
    assert_eq!(vec[0], 1);
    assert_eq!(vec[4], 5);

    TestResult::Ok
}

#[def_test]
fn test_into_inner_with_modified_value() -> TestResult {
    let lock = SpinRaw::new(0);

    {
        let mut guard = lock.lock();
        *guard = 999;
    }

    let value = lock.into_inner();
    assert_eq!(value, 999);

    TestResult::Ok
}


#[def_test]
fn test_spinlock_with_drop_logic() -> TestResult {
    static DROP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker {
        id: usize,
    }

    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNTER.fetch_add(self.id, Ordering::SeqCst);
        }
    }

    DROP_COUNTER.store(0, Ordering::SeqCst);

    let lock = SpinRaw::new(DropTracker { id: 10 });

    {
        let mut guard = lock.lock();
        guard.id = 20;
    }

    // Drop via into_inner
    let _tracker = lock.into_inner();
    assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 0);

    drop(_tracker);
    assert_eq!(DROP_COUNTER.load(Ordering::SeqCst), 20);

    TestResult::Ok
}

#[def_test]
fn test_spinlock_sequential_modifications() -> TestResult {
    let lock = SpinRaw::new(1);

    // Sequential modifications
    for i in 2..=10 {
        let mut guard = lock.lock();
        *guard *= i;
    }

    let guard = lock.lock();
    // 1 * 2 * 3 * 4 * 5 * 6 * 7 * 8 * 9 * 10 = 3628800
    assert_eq!(*guard, 3628800);

    TestResult::Ok
}

#[def_test]
fn test_spinlock_alternating_access_pattern() -> TestResult {
    let lock = SpinRaw::new(Vec::<usize>::new());

    // Alternating push and check
    for i in 0..10 {
        {
            let mut guard = lock.lock();
            guard.push(i);
        }

        {
            let guard = lock.lock();
            assert_eq!(guard.len(), i + 1);
            assert_eq!(guard[i], i);
        }
    }

    {
        let guard = lock.lock();
        assert_eq!(guard.len(), 10);
        assert_eq!(*guard, alloc::vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    TestResult::Ok
}


#[def_test]
fn test_spinlock_array_type() -> TestResult {
    // Test with array type
    let lock = SpinRaw::new([1, 2, 3, 4, 5]);

    {
        let mut guard = lock.lock();
        assert_eq!(guard.len(), 5);
        guard[0] = 10;
        guard[4] = 50;
    }

    {
        let guard = lock.lock();
        assert_eq!(guard[0], 10);
        assert_eq!(guard[4], 50);
        assert_eq!(guard[1], 2);
    }

    TestResult::Ok
}

#[def_test]
fn test_spinlock_reference_semantics() -> TestResult {
    let lock = SpinRaw::new(alloc::vec![1, 2, 3]);

    {
        let guard = lock.lock();
        let slice: &[i32] = &guard;
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0], 1);
    }

    {
        let mut guard = lock.lock();
        let slice: &mut [i32] = &mut guard;
        slice[0] = 99;
    }

    {
        let guard = lock.lock();
        assert_eq!(guard[0], 99);
    }

    TestResult::Ok
}
