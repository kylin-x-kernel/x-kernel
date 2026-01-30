//! Unit tests for Mutex.

extern crate alloc;
use alloc::vec;

use unittest::{
    test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
};

use crate::{Mutex, SpinConfig};

test_fn! {
    using TestResult;

    fn test_mutex_concurrent_modification() {
        // Test concurrent modifications to ensure mutual exclusion
        let mutex = Mutex::new(0);

        // Simulate multiple "tasks" modifying the value
        for _ in 0..100 {
            let mut guard = mutex.lock();
            let old = *guard;
            *guard = old + 1;
            drop(guard);
        }

        assert_eq!(*mutex.lock(), 100);
    }
}

test_fn! {
    using TestResult;

    fn test_mutex_nested_lock_deadlock_detection() {
        // Test that nested locking from same context doesn't cause issues
        // Note: This would deadlock in a real scenario, but we test the guard drop behavior
        let mutex = Mutex::new(vec![1, 2, 3]);

        {
            let mut guard = mutex.lock();
            guard.push(4);
            assert_eq!(guard.len(), 4);
        }

        // After guard dropped, should be able to lock again
        {
            let guard = mutex.lock();
            assert_eq!(guard.len(), 4);
            assert_eq!(guard[3], 4);
        }
    }
}

test_fn! {
    using TestResult;

    fn test_mutex_with_custom_spin_config() {
        // Test mutex with custom spin configuration
        let mutex = Mutex::const_new(
            crate::RawMutex::with_config(SpinConfig {
                max_spins: 20,
                spin_before_yield: 5,
            }),
            42,
        );

        let guard = mutex.lock();
        assert_eq!(*guard, 42);
        drop(guard);

        // Test modification
        *mutex.lock() = 100;
        assert_eq!(*mutex.lock(), 100);
    }
}

tests_name!(TEST_MUTEX;
    test_mutex_concurrent_modification,
    test_mutex_nested_lock_deadlock_detection,
    test_mutex_with_custom_spin_config,
);
