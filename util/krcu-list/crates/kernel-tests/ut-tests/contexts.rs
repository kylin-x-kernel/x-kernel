// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use krcu_list::{Entry, RcuList};
use unittest::{assert, assert_eq, def_test};

#[def_test(serial)]
fn controlled_nmi_traverses_while_writer_is_locked() {
    let list = RcuList::new();
    for value in 0..4 {
        // SAFETY: initialized task with no reentrant writer.
        unsafe { list.push_back(Entry::new(value)) };
    }
    let mut is_valid = false;
    let mut predicate_fn = |value: &usize| {
        if *value != 1 {
            return false;
        }
        let _irq = kspin::IrqSave::new();
        // SAFETY: writer lock excludes preemption; IRQ guard excludes
        // normal IRQs, current is initialized. Balanced software NMI entry
        // only reads and never allocates, schedules or reacquires the lock.
        is_valid = unsafe {
            kirq::context::test_support::with_nmi_context(|| {
                krcu::with_interrupt_read(|scope| {
                    kirq::context::is_in_nmi()
                        && list.iter(scope).copied().eq(0..4)
                        && krcu::with_interrupt_read(|inner| list.iter(inner).count() == 4)
                })
            })
        };
        true
    };
    // SAFETY: admitted task, primitive nonblocking destruction. Predicate only
    // runs balanced controlled NMI reads and restores its original IRQ state.
    let removed = unsafe { list.remove_first(&mut predicate_fn) };
    assert_eq!(removed, Ok(true));
    assert!(is_valid);
    assert!(!kirq::context::is_in_nmi());
    // SAFETY: no active scope or lock remains; callbacks need no test resources.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
}

#[def_test(serial)]
fn real_timer_irq_reads_published_then_removed_list() {
    let list = Arc::new(RcuList::new());
    // SAFETY: initialized task context.
    unsafe { list.push_back(Entry::new(17)) };
    for expected_count in [1, 0] {
        let completed = Arc::new(AtomicBool::new(false));
        let is_valid = Arc::new(AtomicBool::new(false));
        let timer = ktask::Timer::new({
            let list = list.clone();
            let completed = completed.clone();
            let is_valid = is_valid.clone();
            move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
                let observed = kirq::context::interrupt_context_level()
                    == kirq::context::InterruptContextLevel::Hardirq
                    && !karch::local_irq_enabled()
                    && krcu::with_interrupt_read(|scope| {
                        list.iter(scope).count() == expected_count
                            && list.iter(scope).all(|value| *value == 17)
                    });
                is_valid.store(observed, Ordering::Relaxed);
                completed.store(true, Ordering::Release);
            }
        });
        // SAFETY: initialized one-shot timer; callback performs nonblocking reads
        // and atomic reporting, and retains its own list ownership.
        let started = unsafe {
            timer.start(khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(2))
        };
        if started.is_ok() {
            // Event acknowledgement, not elapsed guest time, advances the test.
            // The outer QEMU timeout diagnoses missing delivery.
            while !completed.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
        timer.cancel_sync();
        drop(timer);
        assert!(started.is_ok());
        assert!(is_valid.load(Ordering::Acquire));
        if expected_count == 1 {
            // SAFETY: sleepable task, primitive release and no active scopes.
            assert_eq!(unsafe { list.remove_first(|_| true) }, Ok(true));
        }
    }
    // SAFETY: timer captures released, no callback waits for this task.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
}

struct ReadOnDrop {
    observed: Arc<RcuList<usize>>,
    is_valid: Arc<AtomicBool>,
}

impl Drop for ReadOnDrop {
    fn drop(&mut self) {
        let is_softirq = kirq::context::interrupt_context_level()
            == kirq::context::InterruptContextLevel::Softirq;
        let valid = is_softirq
            && krcu::with_interrupt_read(|scope| self.observed.iter(scope).copied().eq([17, 23]));
        self.is_valid.store(valid, Ordering::Release);
    }
}

#[def_test(serial)]
fn real_retirement_callback_can_read_another_list() {
    let observed = Arc::new(RcuList::new());
    for value in [17, 23] {
        // SAFETY: initialized task context with no reentrant mutation.
        unsafe { observed.push_back(Entry::new(value)) };
    }
    let is_valid = Arc::new(AtomicBool::new(false));
    let retired = RcuList::new();
    // SAFETY: task context; callback captures only independently owned state.
    unsafe {
        retired.push_back(Entry::new(ReadOnDrop {
            observed: observed.clone(),
            is_valid: is_valid.clone(),
        }))
    };
    // SAFETY: destructor performs only watched interrupt reads, an atomic store
    // and nonblocking Arc/Box release. It never waits for this test or service.
    assert_eq!(unsafe { retired.remove_first(|_| true) }, Ok(true));
    drop(retired);
    // SAFETY: no scope/lock held; observed remains alive independently.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
    assert!(is_valid.load(Ordering::Acquire));
}
