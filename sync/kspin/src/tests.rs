//! Test suite for kspin

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::channel,
    },
    thread,
};

use super::*;

struct TestGuardIrq;

static mut IRQ_CNT: u32 = 0;

impl BaseGuard for TestGuardIrq {
    type State = u32;

    fn acquire() -> Self::State {
        unsafe {
            IRQ_CNT += 1;
            IRQ_CNT
        }
    }

    fn release(_: Self::State) {
        unsafe {
            IRQ_CNT -= 1;
        }
    }
}

type TestSpinIrq<T> = SpinLock<TestGuardIrq, T>;
type TestMutex<T> = SpinRaw<T>;

#[derive(Eq, PartialEq, Debug)]
struct NonCopy(i32);

#[test]
fn smoke() {
    let m = TestMutex::new(());
    drop(m.lock());
    drop(m.lock());
}

#[test]
#[cfg(feature = "smp")]
fn concurrent_increments() {
    static M: TestMutex<()> = TestMutex::new(());
    static mut CNT: u32 = 0;
    const INCREMENTS_PER_THREAD: u32 = 1000;
    const NUM_THREADS: u32 = 3;

    fn inc() {
        for _ in 0..INCREMENTS_PER_THREAD {
            unsafe {
                let _g = M.lock();
                CNT += 1;
            }
        }
    }

    let (tx, rx) = channel();
    let mut dispatch_irqs = Vec::new();

    for _ in 0..NUM_THREADS * 2 {
        let tx = tx.clone();
        dispatch_irqs.push(thread::spawn(move || {
            inc();
            tx.send(()).unwrap();
        }));
    }

    drop(tx);
    for _ in 0..NUM_THREADS * 2 {
        rx.recv().unwrap();
    }

    assert_eq!(unsafe { CNT }, INCREMENTS_PER_THREAD * NUM_THREADS * 2);

    for h in dispatch_irqs {
        h.join().unwrap();
    }
}

#[test]
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

#[test]
fn guard_state_restored() {
    let m = TestSpinIrq::new(());
    let _a = m.lock();
    assert_eq!(unsafe { IRQ_CNT }, 1);
    drop(_a);
    assert_eq!(unsafe { IRQ_CNT }, 0);
}

#[test]
#[cfg(feature = "smp")]
fn failed_try_lock_restores_state() {
    let m = TestSpinIrq::new(());
    let _a = m.lock();
    assert_eq!(unsafe { IRQ_CNT }, 1);

    let b = m.try_lock();
    assert!(b.is_none());
    assert_eq!(unsafe { IRQ_CNT }, 1);

    drop(_a);
    assert_eq!(unsafe { IRQ_CNT }, 0);
}

#[test]
fn into_inner_works() {
    let m = TestMutex::new(NonCopy(10));
    assert_eq!(m.into_inner(), NonCopy(10));
}

#[test]
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

#[test]
fn nested_locks() {
    let arc = Arc::new(TestMutex::new(1));
    let arc2 = Arc::new(TestMutex::new(arc));
    let (tx, rx) = channel();

    let t = thread::spawn(move || {
        let lock = arc2.lock();
        let lock2 = lock.lock();
        assert_eq!(*lock2, 1);
        tx.send(()).unwrap();
    });

    rx.recv().unwrap();
    t.join().unwrap();
}

#[test]
fn unwind_safety() {
    let arc = Arc::new(TestMutex::new(1));
    let arc2 = arc.clone();

    let _ = thread::spawn(move || {
        struct Unwinder {
            i: Arc<TestMutex<i32>>,
        }
        impl Drop for Unwinder {
            fn drop(&mut self) {
                *self.i.lock() += 1;
            }
        }
        let _u = Unwinder { i: arc2 };
        panic!();
    })
    .join();

    let lock = arc.lock();
    assert_eq!(*lock, 2);
}

#[test]
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

#[test]
fn force_unlock_works() {
    let lock = TestMutex::new(());
    std::mem::forget(lock.lock());

    unsafe {
        lock.force_unlock();
    }

    assert!(lock.try_lock().is_some());
}

#[test]
fn debug_output() {
    let lock = TestMutex::new(42);
    let debug_str = format!("{:?}", lock);
    assert!(debug_str.contains("42") || debug_str.contains("SpinLock"));
}
