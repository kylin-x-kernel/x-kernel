// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Actual delivery and explicitly labelled controlled-context fixtures.

use alloc::{boxed::Box, sync::Arc};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kirq::context::InterruptContextLevel;
use krcu::{CallbackNode, PreparedCallback};
use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

use kxarray::XArray;

#[def_test(serial)]
fn timer_hardirq_reads_replacement_absence_and_reinsertion() {
    let array = Arc::new(XArray::<Box<usize>>::new());
    // SAFETY: initialized task, primitive payloads and nonblocking Box release.
    assert!(unsafe { array.insert(0, Box::new(17)) }.is_ok());
    // SAFETY: same task context; growing the root retains the first association.
    assert!(unsafe { array.insert(usize::MAX, Box::new(1)) }.is_ok());
    let requested = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let has_invalid_read = Arc::new(AtomicBool::new(false));
    let irq_array = array.clone();
    let irq_requested = requested.clone();
    let irq_completed = completed.clone();
    let irq_invalid = has_invalid_read.clone();
    let timer = ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
        if kirq::context::interrupt_context_level() != InterruptContextLevel::Hardirq
            || !krcu_core::RcuContextIf::current_cpu_watching().is_watching()
            || karch::local_irq_enabled()
        {
            irq_invalid.store(true, Ordering::Release);
            return;
        }
        let request = irq_requested.load(Ordering::Acquire);
        let expected = match request {
            1 => Some(1),
            2 => Some(2),
            3 => None,
            4 => Some(3),
            _ => {
                irq_invalid.store(true, Ordering::Release);
                return;
            }
        };
        let is_valid = krcu::with_interrupt_read(|scope| {
            irq_array.read(scope, 0) == Some(&17)
                && irq_array.read(scope, usize::MAX).copied() == expected
        });
        if !is_valid {
            irq_invalid.store(true, Ordering::Release);
        }
        irq_completed.store(request, Ordering::Release);
    });
    let mut is_valid = true;
    for stage in 0..4 {
        requested.store(stage + 1, Ordering::Release);
        // SAFETY: initialized timer service; one-shot callback only reads
        // and reports atomics. Previous invocation ended before this start.
        if unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(2)) }.is_err()
        {
            is_valid = false;
            break;
        }
        // Advance only after this invocation reports completion or invalid data.
        // The external QEMU timeout detects missing delivery, not guest latency.
        while completed.load(Ordering::Acquire) != stage + 1
            && !has_invalid_read.load(Ordering::Acquire)
        {
            ktask::yield_now();
        }
        // No callback may retain this request across the next mutation.
        timer.cancel_sync();
        if completed.load(Ordering::Acquire) != stage + 1
            || has_invalid_read.load(Ordering::Acquire)
        {
            is_valid = false;
            break;
        }
        let updated = match stage {
            // SAFETY: watched sleepable task outside scopes/bookkeeping locks,
            // permanent RCU service and primitive nonblocking release.
            0 => unsafe { array.set(usize::MAX, Box::new(2)) }.is_ok(),
            // SAFETY: same task context, preflight precedes withdrawal.
            1 => (unsafe { array.remove(usize::MAX) }) == Ok(true),
            // SAFETY: same context and primitive adoption contract.
            2 => unsafe { array.insert(usize::MAX, Box::new(3)) }.is_ok(),
            _ => true,
        };
        if !updated {
            is_valid = false;
            break;
        }
    }
    timer.cancel_sync();
    drop(timer);
    drop(array);
    // SAFETY: timer captures released, no active read scope or required lock.
    let drained = unsafe { krcu::drain_callbacks() };
    assert!(is_valid);
    assert!(!has_invalid_read.load(Ordering::Acquire));
    assert_eq!(completed.load(Ordering::Acquire), 4);
    assert!(drained.is_ok());
}

#[repr(C)]
struct SoftirqRead {
    node: CallbackNode,
    array: Arc<XArray<Box<usize>>>,
    expected: Option<usize>,
    outcome: Arc<AtomicUsize>,
}

const _: () = core::assert!(core::mem::offset_of!(SoftirqRead, node) == 0);

/// # Safety
/// The pointer is the zero-offset projection of one submitted SoftirqRead Box,
/// invoked exactly once after service detachment. Invocation consumes the record.
unsafe fn read_from_softirq(node: *mut CallbackNode) {
    // SAFETY: checked zero offset preserves Box provenance and unique ownership.
    let record = unsafe { Box::from_raw(node.cast::<SoftirqRead>()) };
    let is_context_valid = kirq::context::interrupt_context_level()
        == InterruptContextLevel::Softirq
        && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
        && !ktask::is_sleepable_context();
    let is_valid = is_context_valid
        && krcu::with_interrupt_read(|scope| {
            let outer = record.array.read(scope, usize::MAX).copied();
            let inner =
                krcu::with_interrupt_read(|nested| record.array.read(nested, usize::MAX).copied());
            outer == record.expected && inner == record.expected
        });
    let outcome = record.outcome.clone();
    // Drop the captured array share before exposing callback completion.
    drop(record);
    outcome.store(if is_valid { 1 } else { 2 }, Ordering::Release);
}

/// # Safety
/// The pointer is the zero-offset field of one rejected/unsubmitted record.
/// The task retains another array owner; cancellation owns only this record.
unsafe fn cancel_softirq_read(node: *mut CallbackNode) {
    // SAFETY: checked layout and sole cancellation ownership recover the Box.
    drop(unsafe { Box::from_raw(node.cast::<SoftirqRead>()) });
}

fn check_softirq_read(array: &Arc<XArray<Box<usize>>>, expected: Option<usize>) -> bool {
    let outcome = Arc::new(AtomicUsize::new(0));
    let raw = Box::into_raw(Box::new(SoftirqRead {
        node: CallbackNode::new(read_from_softirq),
        array: array.clone(),
        expected,
        outcome: outcome.clone(),
    }));
    // SAFETY: raw field projection preserves enclosing allocation provenance.
    let node = unsafe { &raw mut (*raw).node };
    // SAFETY: field of a live allocation is nonnull.
    let node = unsafe { NonNull::new_unchecked(node) };
    // SAFETY: unique stable record, transferable captures, nonblocking callback
    // and cancellation. The task retains the container throughout completion.
    let prepared = unsafe { PreparedCallback::new(node, cancel_softirq_read) };
    // SAFETY: initialized watched task, unpublished record, no locks/read scope.
    if let Err((_, rejected)) = unsafe { prepared.submit() } {
        drop(rejected);
        return false;
    }
    // SAFETY: callback reads only the array, never awaits this task; no exclusion
    // is retained. The drain completes callback execution, not just eligibility.
    let drained = unsafe { krcu::drain_callbacks() };
    drained.is_ok() && outcome.load(Ordering::Acquire) == 1
}

#[def_test(serial)]
fn serving_softirq_reads_live_and_removed_entries() {
    let array = Arc::new(XArray::<Box<usize>>::new());
    // SAFETY: initialized task, nonblocking primitive Box release.
    assert!(unsafe { array.insert(usize::MAX, Box::new(7)) }.is_ok());
    assert!(check_softirq_read(&array, Some(7)));
    // SAFETY: watched task outside scopes/locks with permanent RCU service.
    assert_eq!(unsafe { array.remove(usize::MAX) }, Ok(true));
    assert!(check_softirq_read(&array, None));
    // SAFETY: initialized task and primitive payload adoption contract.
    assert!(unsafe { array.insert(usize::MAX, Box::new(9)) }.is_ok());
    assert!(check_softirq_read(&array, Some(9)));
}

#[def_test(serial)]
fn writer_exclusion_keeps_normal_interrupts_enabled() {
    let array = XArray::<Box<usize>>::new();
    assert!(karch::local_irq_enabled());
    let is_enabled = array.with_writer_locked_for_test(karch::local_irq_enabled);
    assert!(is_enabled);
    assert!(karch::local_irq_enabled());
}

#[def_test(serial)]
fn controlled_nmi_reads_while_writer_lock_is_held() {
    let array = XArray::<Box<usize>>::new();
    // SAFETY: initialized task, primitive payload and nonblocking Box release.
    assert!(unsafe { array.insert(0, Box::new(17)) }.is_ok());
    for expected in [Some(7), Some(9), None, Some(11)] {
        let updated = match expected {
            // SAFETY: watched task outside scopes/locks, primitive ownership,
            // permanent RCU service and no explicit submission context change.
            Some(value) => unsafe { array.set(usize::MAX, Box::new(value)) }.is_ok(),
            // SAFETY: same admitted task context; retirement is outside exclusion.
            None => (unsafe { array.remove(usize::MAX) }) == Ok(true),
        };
        let is_valid = {
            let pin = kspin::NoPreemptIrqSave::new();
            let task = ktask::current();
            // SAFETY: task retains current storage while IRQ/preemption are
            // excluded. This fixture only inspects its atomic reader state.
            let reader = unsafe { task.rcu_reader_for_integration() };
            let cpu = khal::percpu::this_cpu_id();
            let before = (
                reader.is_reading(),
                reader.is_enrolled(),
                krcu::reader_cleanup_requested_for_tests(cpu),
            );
            let allocated_before = kalloc::current_cpu_allocated_bytes_for_test(&pin);
            let observed = array.with_writer_locked_for_test(|| {
                // SAFETY: retained initialized current, IRQ/preemption excluded;
                // balanced software NMI entry establishes watching. Only scoped
                // reads and atomic observations execute, never scheduling/locking.
                unsafe {
                    kirq::context::test_support::with_nmi_context(|| {
                        let is_nmi = kirq::context::is_in_nmi();
                        krcu::with_interrupt_read(|scope| {
                            let outer = array.read(scope, usize::MAX);
                            let nested_matches = krcu::with_interrupt_read(|nested| {
                                let inner = array.read(nested, usize::MAX);
                                match (outer, inner) {
                                    (Some(a), Some(b)) => core::ptr::eq(a, b),
                                    (None, None) => true,
                                    _ => false,
                                }
                            });
                            is_nmi
                                && reader.is_reading()
                                && nested_matches
                                && outer.copied() == expected
                                && array.read(scope, 0) == Some(&17)
                                && array.read(scope, 64).is_none()
                        })
                    })
                }
            });
            let allocated_after = kalloc::current_cpu_allocated_bytes_for_test(&pin);
            observed
                && allocated_before == allocated_after
                && !kirq::context::is_in_nmi()
                && !karch::local_irq_enabled()
                && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
                && before
                    == (
                        reader.is_reading(),
                        reader.is_enrolled(),
                        krcu::reader_cleanup_requested_for_tests(cpu),
                    )
        };
        assert!(updated);
        assert!(is_valid);
    }
    drop(array);
    // SAFETY: no scopes/locks retained, callbacks own only primitive allocations.
    assert!(unsafe { krcu::drain_callbacks() }.is_ok());
}

struct PublicationProbe {
    value: usize,
    observations: AtomicUsize,
    has_invalid_read: AtomicBool,
}

impl PublicationProbe {
    fn new(value: usize) -> Box<Self> {
        Box::new(Self {
            value,
            observations: AtomicUsize::new(0),
            has_invalid_read: AtomicBool::new(false),
        })
    }
}

fn observe_partial_publication(array: &XArray<Box<PublicationProbe>>, index: usize) {
    // Software NMI entry needs IRQ exclusion; writer serialization itself does
    // not mask ordinary interrupts.
    let _irq = kspin::IrqSave::new();
    // SAFETY: invoked under the writer lock with current/watching initialized
    // and IRQ/preemption excluded. Balanced software NMI performs only scoped
    // reads and atomic reporting, with no allocation, scheduling or mutation.
    unsafe {
        kirq::context::test_support::with_nmi_context(|| {
            krcu::with_interrupt_read(|scope| {
                let Some(anchor) = array.read(scope, 0) else {
                    return;
                };
                let expected_far = (index == 64).then_some(23);
                let is_valid = kirq::context::is_in_nmi()
                    && !karch::local_irq_enabled()
                    && anchor.value == 17
                    && array.read(scope, index).is_none()
                    && array.read(scope, usize::MAX).map(|value| value.value) == expected_far
                    && krcu::with_interrupt_read(|nested| array.read(nested, index).is_none());
                if !is_valid {
                    anchor.has_invalid_read.store(true, Ordering::Release);
                }
                anchor.observations.fetch_add(1, Ordering::Release);
            });
        })
    }
}

#[def_test(serial)]
fn controlled_nmi_reads_before_root_and_payload_publication() {
    let array = XArray::<Box<PublicationProbe>>::new();
    // SAFETY: task context; atomic payloads and nonblocking allocator release.
    assert!(unsafe { array.insert(0, PublicationProbe::new(17)) }.is_ok());
    // First: new ancestors remain private until the final root publication.
    // Second: the existing tree exposes a newly linked, still-empty leaf.
    for (stage, index) in [usize::MAX, 64].into_iter().enumerate() {
        array.set_publication_hook_for_test(observe_partial_publication);
        // SAFETY: same task/adoption contract; test observer restores context
        // and only reads while the insertion holds its writer lock.
        let inserted = unsafe { array.insert(index, PublicationProbe::new(23)) }.is_ok();
        let is_valid = krcu::with_read(|scope| {
            array.read(scope, 0).is_some_and(|anchor| {
                anchor.observations.load(Ordering::Acquire) == stage + 1
                    && !anchor.has_invalid_read.load(Ordering::Acquire)
            }) && array
                .read(scope, index)
                .is_some_and(|value| value.value == 23)
        });
        assert!(inserted && is_valid);
        assert!(!kirq::context::is_in_nmi());
    }
}
