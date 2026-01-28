use std::sync::{Arc, Once};

use ksync::RwLock;
use ktask as thread;

static INIT: Once = Once::new();

#[test]
fn rwlock_basic() {
    INIT.call_once(thread::init_scheduler);

    let lock = RwLock::new(0);

    {
        let r = lock.read();
        assert_eq!(*r, 0);
    }

    {
        let mut w = lock.write();
        *w = 42;
    }

    {
        let r = lock.read();
        assert_eq!(*r, 42);
    }
}

#[test]
fn rwlock_multiple_readers() {
    INIT.call_once(thread::init_scheduler);

    let lock = Arc::new(RwLock::new(0));
    let mut handles = vec![];

    for _ in 0..10 {
        let lock = lock.clone();
        let handle = thread::spawn(move || {
            let _r = lock.read();
            // multiple readers can access simultaneously
            thread::yield_now();
        });
        handles.push(handle);
    }

    for h in handles {
        h.join();
    }
}

#[test]
fn rwlock_writer_exclusive() {
    INIT.call_once(thread::init_scheduler);

    static LOCK: RwLock<u32> = RwLock::new(0);

    let w = LOCK.write();
    // Writer blocks readers
    assert!(LOCK.try_read().is_none());
    // Writer blocks other writers
    assert!(LOCK.try_write().is_none());
    drop(w);

    // Now readers can access
    assert!(LOCK.try_read().is_some());
}

#[test]
fn rwlock_concurrent_reads_and_writes() {
    INIT.call_once(thread::init_scheduler);

    static LOCK: RwLock<u32> = RwLock::new(0);
    let mut handles = vec![];

    // Spawn writers
    for i in 0..5 {
        let handle = thread::spawn(move || {
            for _ in 0..100 {
                let mut w = LOCK.write();
                *w += 1;
                thread::yield_now();
            }
        });
        handles.push(handle);
    }

    // Spawn readers
    for _ in 0..5 {
        let handle = thread::spawn(move || {
            for _ in 0..100 {
                let r = LOCK.read();
                let _val = *r;
                thread::yield_now();
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.join();
    }

    assert_eq!(*LOCK.read(), 500);
}

#[test]
fn rwlock_try_lock() {
    INIT.call_once(thread::init_scheduler);

    let lock = RwLock::new(0);

    // Can acquire read lock
    let r1 = lock.try_read();
    assert!(r1.is_some());

    // Can acquire multiple read locks
    let r2 = lock.try_read();
    assert!(r2.is_some());

    // Cannot acquire write lock while readers exist
    assert!(lock.try_write().is_none());

    drop(r1);
    drop(r2);

    // Can acquire write lock now
    let w = lock.try_write();
    assert!(w.is_some());

    // Cannot acquire read lock while writer exists
    assert!(lock.try_read().is_none());
}
