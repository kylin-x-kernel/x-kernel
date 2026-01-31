//! Unit tests for RwLock.

extern crate alloc;
use alloc::vec;

use unittest::{assert_eq, def_test};

use crate::RwLock;

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
