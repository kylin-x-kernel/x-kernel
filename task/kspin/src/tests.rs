// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test suite for kspin

#![cfg(unittest)]

use alloc::{format, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

struct TestGuardIrq;

static IRQ_CNT: AtomicU32 = AtomicU32::new(0);

impl BaseGuard for TestGuardIrq {
    type State = u32;

    fn acquire() -> Self::State {
        IRQ_CNT.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn release(_: Self::State) {
        IRQ_CNT.fetch_sub(1, Ordering::Relaxed);
    }
}

type TestSpinIrq<T> = SpinLock<TestGuardIrq, T>;
type TestRwSpinIrq<T> = SpinRwLock<TestGuardIrq, T>;
type TestMutex<T> = SpinRaw<T>;

#[derive(Eq, PartialEq, Debug)]
struct NonCopy(i32);

#[def_test]
fn smoke() {
    let m = TestMutex::new(());
    drop(m.lock());
    drop(m.lock());
}

#[def_test]
#[cfg(feature = "smp")]
fn try_lock_works() {
    let mutex = TestMutex::new(42);

    let a = mutex.try_lock();
    assert_eq!(a.as_ref().map(|r| **r), Some(42));

    let b = mutex.try_lock();
    assert!(b.is_none());

    drop(a);
    let c = mutex.try_lock();
    assert_eq!(c.as_ref().map(|r| **r), Some(42));
}

#[def_test(serial)]
fn guard_state_restored() {
    let m = TestSpinIrq::new(());
    let _a = m.lock();
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 1);
    drop(_a);
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn rwlock_allows_multiple_readers() {
    let lock = TestRwSpinIrq::new(42);
    let first = lock.read();
    let second = lock.read();

    assert_eq!(*first, 42);
    assert_eq!(*second, 42);
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 2);

    drop(first);
    drop(second);
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn rwlock_writer_excludes_readers_and_writers() {
    let lock = TestRwSpinIrq::new(1);
    let mut writer = lock.write();
    *writer = 2;

    assert!(lock.try_read().is_none());
    assert!(lock.try_write().is_none());
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 1);

    drop(writer);
    assert_eq!(*lock.read(), 2);
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
fn rwlock_try_write_fails_when_reader_is_active() {
    let lock = TestRwSpinIrq::new(());
    let reader = lock.read();

    assert!(lock.try_write().is_none());
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 1);

    drop(reader);
    assert!(lock.try_write().is_some());
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 0);
}

#[def_test(serial)]
#[cfg(feature = "smp")]
fn failed_try_lock_restores_state() {
    let m = TestSpinIrq::new(());
    let _a = m.lock();
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 1);

    let b = m.try_lock();
    assert!(b.is_none());
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 1);

    drop(_a);
    assert_eq!(IRQ_CNT.load(Ordering::Relaxed), 0);
}

#[def_test]
fn into_inner_works() {
    let m = TestMutex::new(NonCopy(10));
    assert_eq!(m.into_inner(), NonCopy(10));
}

#[def_test]
fn into_inner_drops() {
    struct Foo(Arc<AtomicUsize>);
    impl Drop for Foo {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let num_drops = Arc::new(AtomicUsize::new(0));
    let m = TestMutex::new(Foo(num_drops.clone()));
    assert_eq!(num_drops.load(Ordering::SeqCst), 0);

    {
        let _inner = m.into_inner();
        assert_eq!(num_drops.load(Ordering::SeqCst), 0);
    }

    assert_eq!(num_drops.load(Ordering::SeqCst), 1);
}

#[def_test]
fn nested_locks() {
    let arc = Arc::new(TestMutex::new(1));
    let arc2 = Arc::new(TestMutex::new(arc));

    // Single threaded nested lock test
    let lock = arc2.lock();
    let lock2 = lock.lock();
    assert_eq!(*lock2, 1);
}

#[def_test]
fn unsized_types() {
    let mutex: &TestMutex<[i32]> = &TestMutex::new([1, 2, 3]);
    {
        let mut b = mutex.lock();
        b[0] = 4;
        b[2] = 5;
    }
    let expected: &[i32] = &[4, 2, 5];
    assert_eq!(&*mutex.lock(), expected);
}

#[def_test]
fn force_unlock_works() {
    let lock = TestMutex::new(());
    core::mem::forget(lock.lock());

    // SAFETY: this test intentionally leaks the guard, so the current
    // execution path still owns the lock and may exercise `force_unlock`.
    unsafe {
        lock.force_unlock();
    }

    assert!(lock.try_lock().is_some());
}

#[def_test]
fn debug_output() {
    let lock = TestMutex::new(42);
    let debug_str = format!("{:?}", lock);
    assert!(debug_str.contains("42") || debug_str.contains("SpinLock"));
}
