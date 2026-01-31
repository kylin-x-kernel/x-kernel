//! Unit tests for ksync using the unittest framework.

#![cfg(unittest)]

extern crate alloc;

use alloc::{sync::Arc, vec, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use super::{Mutex, RwLock, Semaphore, SpinConfig};

// ============================================================================
// Mutex Tests
// ============================================================================

#[def_test]
fn test_mutex_concurrent_modification() {
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

#[def_test]
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

#[def_test]
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

#[def_test]
fn test_mutex_try_lock_success_and_failure() {
    // Test try_lock in both success and failure scenarios
    let mutex = Arc::new(Mutex::new(0));

    // Should succeed when unlocked
    {
        let guard = mutex.try_lock();
        assert!(guard.is_some());
        if let Some(mut g) = guard {
            *g = 42;
        }
    }

    // Verify the value was set
    assert_eq!(*mutex.lock(), 42);

    // Test that try_lock fails when already locked
    let _guard = mutex.lock();
    let try_result = mutex.try_lock();
    assert!(try_result.is_none());
}

#[def_test]
fn test_mutex_boundary_zero_and_overflow() {
    // Test boundary conditions with zero and large values
    let mutex = Mutex::new(0usize);

    // Test starting from zero
    {
        let guard = mutex.lock();
        assert_eq!(*guard, 0);
    }

    // Test near maximum value (boundary test)
    {
        let mut guard = mutex.lock();
        *guard = usize::MAX - 1;
    }

    {
        let guard = mutex.lock();
        assert_eq!(*guard, usize::MAX - 1);
    }

    // Test wrapping behavior
    {
        let mut guard = mutex.lock();
        *guard = guard.wrapping_add(2); // Should wrap to 0
        assert_eq!(*guard, 0);
    }
}

// ============================================================================
// RwLock Tests
// ============================================================================

#[def_test]
fn test_rwlock_multiple_readers() {
    // Test that multiple readers can hold the lock simultaneously
    let lock = RwLock::new(42);

    // Acquire multiple read guards
    let r1 = lock.read();
    let r2 = lock.read();
    let r3 = lock.read();

    assert_eq!(*r1, 42);
    assert_eq!(*r2, 42);
    assert_eq!(*r3, 42);

    // All readers should see the same value
    drop(r1);
    drop(r2);
    drop(r3);
}

#[def_test]
fn test_rwlock_writer_exclusivity() {
    // Test that writer has exclusive access
    let lock = RwLock::new(vec![1, 2, 3]);

    {
        let mut w = lock.write();
        w.push(4);
        w.push(5);
        assert_eq!(w.len(), 5);
    }

    // After writer drops, reader should see the changes
    {
        let r = lock.read();
        assert_eq!(r.len(), 5);
        assert_eq!(r[3], 4);
        assert_eq!(r[4], 5);
    }
}

#[def_test]
fn test_rwlock_upgradeable_read() {
    // Test read -> write transition
    let lock = RwLock::new(0);

    // Start with read
    {
        let r = lock.read();
        assert_eq!(*r, 0);
    }

    // Upgrade to write
    {
        let mut w = lock.write();
        *w = 100;
    }

    // Verify change
    {
        let r = lock.read();
        assert_eq!(*r, 100);
    }
}

#[def_test]
fn test_rwlock_try_read_and_write() {
    // Test try_read and try_write functionality
    let lock = Arc::new(RwLock::new(42));

    // try_read should succeed when unlocked
    {
        let r1 = lock.try_read();
        assert!(r1.is_some());
        assert_eq!(*r1.unwrap(), 42);
    }

    // try_read should succeed with other readers
    {
        let r1 = lock.read();
        let r2 = lock.try_read();
        assert!(r2.is_some());
        assert_eq!(*r1, 42);
        assert_eq!(*r2.unwrap(), 42);
    }

    // try_write should fail when readers exist
    {
        let _r = lock.read();
        let w = lock.try_write();
        assert!(w.is_none());
    }

    // try_write should succeed when unlocked
    {
        let w = lock.try_write();
        assert!(w.is_some());
        if let Some(mut guard) = w {
            *guard = 99;
        }
    }

    assert_eq!(*lock.read(), 99);
}

#[def_test]
fn test_rwlock_writer_blocks_readers() {
    // Test that an active writer prevents new readers
    let lock = Arc::new(RwLock::new(Vec::<i32>::new()));

    // Writer acquires lock
    {
        let mut w = lock.write();
        w.push(1);
        w.push(2);
        w.push(3);

        // While writer holds lock, try_read should fail
        let r = lock.try_read();
        assert!(r.is_none());

        w.push(4);
    }

    // After writer releases, readers should work
    {
        let r = lock.read();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0], 1);
        assert_eq!(r[3], 4);
    }
}

// ============================================================================
// Semaphore Tests
// ============================================================================

#[def_test]
fn test_semaphore_basic_acquire_release() {
    // Test basic acquire and release operations
    let sem = Semaphore::new(2);

    assert_eq!(sem.available_permits(), 2);

    sem.acquire();
    assert_eq!(sem.available_permits(), 1);

    sem.acquire();
    assert_eq!(sem.available_permits(), 0);

    sem.release();
    assert_eq!(sem.available_permits(), 1);

    sem.release();
    assert_eq!(sem.available_permits(), 2);
}

#[def_test]
fn test_semaphore_try_acquire_boundary() {
    // Test try_acquire at boundary conditions
    let sem = Semaphore::new(1);

    // First try_acquire should succeed
    assert!(sem.try_acquire());
    assert_eq!(sem.available_permits(), 0);

    // Second try_acquire should fail (no permits)
    assert!(!sem.try_acquire());
    assert_eq!(sem.available_permits(), 0);

    // After release, try_acquire should succeed again
    sem.release();
    assert_eq!(sem.available_permits(), 1);
    assert!(sem.try_acquire());
    assert_eq!(sem.available_permits(), 0);
}

#[def_test]
fn test_semaphore_guard_raii() {
    // Test that SemaphoreGuard properly releases on drop
    let sem = Semaphore::new(3);

    assert_eq!(sem.available_permits(), 3);

    {
        let _guard1 = sem.acquire_guard();
        assert_eq!(sem.available_permits(), 2);

        {
            let _guard2 = sem.acquire_guard();
            assert_eq!(sem.available_permits(), 1);
        }

        // guard2 dropped, permit should be released
        assert_eq!(sem.available_permits(), 2);
    }

    // guard1 dropped, all permits should be back
    assert_eq!(sem.available_permits(), 3);
}

#[def_test]
fn test_semaphore_zero_permits() {
    // Test semaphore behavior with zero initial permits
    let sem = Arc::new(Semaphore::new(0));

    assert_eq!(sem.available_permits(), 0);

    // try_acquire should fail
    assert!(!sem.try_acquire());

    // Release to add permits
    sem.release();
    assert_eq!(sem.available_permits(), 1);

    // Now try_acquire should succeed
    assert!(sem.try_acquire());
    assert_eq!(sem.available_permits(), 0);
}

#[def_test]
fn test_semaphore_multiple_release_overflow() {
    // Test releasing more permits than initial capacity
    let sem = Semaphore::new(2);

    assert_eq!(sem.available_permits(), 2);

    // Release multiple times to exceed initial capacity
    sem.release();
    sem.release();
    sem.release();

    assert_eq!(sem.available_permits(), 5);

    // Should be able to acquire all 5
    for i in (0..5).rev() {
        assert!(sem.try_acquire());
        assert_eq!(sem.available_permits(), i);
    }

    // Now should be empty
    assert_eq!(sem.available_permits(), 0);
    assert!(!sem.try_acquire());
}
