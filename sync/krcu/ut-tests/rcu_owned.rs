// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controlled task and interrupt consumers; the boot RCU service persists.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn owned_container_accepts_send_only_and_shared_payloads() {
    fn require_send<T: Send>() {}
    fn require_sync<T: Sync>() {}

    // Moving exclusive ownership needs Send, not Sync. Sharing additionally
    // permits a different task to recover and destroy the replaced allocation.
    require_send::<Rcu<core::cell::Cell<u64>>>();
    require_send::<Rcu<u64>>();
    require_sync::<Rcu<u64>>();
    require_send::<Rcu<AtomicUsize>>();
    require_sync::<Rcu<AtomicUsize>>();
}

#[def_test]
fn failed_preparation_preserves_publication_and_new_owner() {
    let value = Rcu::new(Box::new(11_u64));
    let new_value = Box::new(22);
    let new_address = &*new_value as *const u64;
    let result = {
        let _irq = kspin::IrqSave::new();
        value.replace_sync(new_value)
    };
    let (error, returned) = result.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(&*returned as *const u64, new_address);
    assert_eq!(*returned, 22);
    let observed = crate::read::with_task_read(|token| {
        // SAFETY: this is the current task's scope and the boot service is live.
        *unsafe { value.read(token) }
    });
    assert_eq!(observed, 11);
}

#[def_test]
fn replacement_inside_read_scope_leaves_both_owners_unchanged() {
    let value = Rcu::new(Box::new(11_u64));
    let (result, observed) = crate::read::with_task_read(|token| {
        let result = value.replace_sync(Box::new(22));
        // SAFETY: current task scope protects this read; only copied data leaves.
        let observed = *unsafe { value.read(token) };
        (result, observed)
    });
    let (error, returned) = result.unwrap_err();
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(*returned, 22);
    assert_eq!(observed, 11);
}

#[derive(Debug, Default)]
struct OwnershipProbe {
    has_reader_entered: AtomicBool,
    can_reader_exit: AtomicBool,
    is_old_value_intact: AtomicBool,
    returned: [AtomicUsize; 2],
    drops: [AtomicUsize; 3],
    has_invalid_drop_context: AtomicBool,
}

#[derive(Debug)]
struct Payload {
    version: usize,
    probe: Arc<OwnershipProbe>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        if !ktask::is_sleepable_context() {
            self.probe.has_invalid_drop_context.store(true, Ordering::Release);
        }
        self.probe.drops[self.version - 1].fetch_add(1, Ordering::Release);
    }
}

fn payload(version: usize, probe: &Arc<OwnershipProbe>) -> Box<Payload> {
    Box::new(Payload { version, probe: probe.clone() })
}

fn version(value: &Rcu<Payload>) -> usize {
    crate::read::with_task_read(|token| {
        // SAFETY: this live container is read only through the current task's
        // scope in kernel execution; its data does not escape the token.
        unsafe { value.read(token) }.version
    })
}

fn replace_in_task(value: &Arc<Rcu<Payload>>, next: usize, probe: &Arc<OwnershipProbe>) -> ktask::KtaskRef {
    let value = value.clone();
    let probe = probe.clone();
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let old = value.replace_sync(payload(next, &probe)).unwrap();
            probe.returned[next - 2].store(old.version, Ordering::Release);
            drop(old);
        },
        "rcu-replace".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    ktask::activate_task(&task);
    task
}

#[def_test(serial)]
fn overlapping_replacements_keep_old_readers_and_return_distinct_owners() {
    // The reader cannot voluntarily yield inside its scope. The current task
    // scheduler always supports preemption and directly enables kspin/preempt;
    // ktask's optional preempt feature is not a capability gate for this test.
    let probe = Arc::new(OwnershipProbe::default());
    let value = Arc::new(Rcu::new(payload(1, &probe)));
    let reader_value = value.clone();
    let reader_probe = probe.clone();
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            crate::read::with_task_read(|token| {
                // SAFETY: the current task's scope protects this retained borrow;
                // involuntary preemption is allowed but no blocking API is used.
                let old = unsafe { reader_value.read(token) };
                reader_probe.has_reader_entered.store(true, Ordering::Release);
                while !reader_probe.can_reader_exit.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
                reader_probe.is_old_value_intact.store(
                    reader_probe.drops[0].load(Ordering::Acquire) == 0 && old.version == 1,
                    Ordering::Release,
                );
            });
        },
        "rcu-old-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    ktask::activate_task(&reader);
    while !probe.has_reader_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let first = replace_in_task(&value, 2, &probe);
    while version(&value) != 2 {
        ktask::yield_now();
    }
    let second = replace_in_task(&value, 3, &probe);
    while version(&value) != 3 {
        ktask::yield_now();
    }
    // An otherwise uncontended CPU need not switch on a reschedule request.
    // Joining a real same-CPU competitor establishes a scheduler boundary
    // before the enrollment check. The reader cannot exit its scope until we
    // release it below, so registration cannot disappear before that check.
    let competitor = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        || {},
        "rcu-reader-competitor".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    competitor.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    ktask::activate_task(&competitor);
    let competitor_status = competitor.join();
    let was_enrolled = crate::test_support::reader(&reader).is_enrolled();
    // Each updater must reach its wait, or publish an erroneous early return.
    // Do not mistake an updater that has not run yet for a correctly blocked one.
    for (index, updater) in [&first, &second].into_iter().enumerate() {
        while updater.state() != ktask::TaskState::Blocked
            && probe.returned[index].load(Ordering::Acquire) == 0
        {
            ktask::yield_now();
        }
    }
    // Two new versions became visible without waiting for the old reader.
    // Capture results, then always release and join before test assertions.
    let before_release = core::array::from_fn::<_, 2, _>(|i| probe.returned[i].load(Ordering::Acquire));
    probe.can_reader_exit.store(true, Ordering::Release);
    let statuses = [reader.join(), first.join(), second.join()];
    let final_version = version(&value);
    drop(value);
    assert_eq!(competitor_status, 0);
    assert!(was_enrolled);
    assert_eq!(statuses, [0, 0, 0]);
    assert_eq!(before_release, [0, 0]);
    assert!(probe.is_old_value_intact.load(Ordering::Acquire));
    assert_eq!(probe.returned[0].load(Ordering::Acquire), 1);
    assert_eq!(probe.returned[1].load(Ordering::Acquire), 2);
    assert_eq!(final_version, 3);
    for count in &probe.drops {
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}

#[def_test]
fn final_container_drop_reclaims_unreplaced_owner() {
    let probe = Arc::new(OwnershipProbe::default());
    let value = Rcu::new(payload(1, &probe));
    assert_eq!(version(&value), 1);
    drop(value);
    assert_eq!(probe.drops[0].load(Ordering::Acquire), 1);
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}

#[derive(Default)]
struct SoftirqReads {
    completed: AtomicUsize,
    seen: AtomicUsize,
    has_invalid_read: AtomicBool,
}

/// Models NMI delivery, not a hardware exception. The caller retains CPU/current
/// and IRQ exclusion, and may have entered a controlled quiescent window.
///
/// # Safety
/// No scheduling or migration may occur until this function returns. Current
/// must be initialized and the caller must retain the container throughout.
unsafe fn read_from_controlled_nmi(value: &Rcu<Payload>) -> (usize, bool) {
    let read_fn = || {
            crate::read::with_interrupt_read(|token| {
                // SAFETY: the audited software NMI entry established watching
                // and retains current. No reference escapes this strict scope.
                let borrowed = unsafe { value.read(token) };
                let version = borrowed.version;
                let intact = (1..=3).contains(&version)
                    && borrowed.probe.drops[version - 1].load(Ordering::Acquire) == 0;
                let nested = crate::read::with_interrupt_read(|inner_token| {
                    // SAFETY: same retained NMI/current and container; this
                    // inner borrow finishes before the outer scope resumes.
                    let inner = unsafe { value.read(inner_token) };
                    core::ptr::eq(inner, borrowed)
                });
                (version, intact && nested && crate::test_support::reader(&ktask::current()).is_reading())
            })
    };
    // SAFETY: caller supplies CPU/current lifetime and exclusion. Only scoped
    // pointer reads and atomic observations run inside the balanced NMI entry.
    unsafe { kirq::context::test_support::with_nmi_context(read_fn) }
}

#[def_test(serial)]
fn controlled_nmi_reads_task_and_quiescent_origin_publications() {
    let probe = Arc::new(OwnershipProbe::default());
    let value = Rcu::new(payload(1, &probe));
    let mut valid = true;
    let mut returned = [0; 2];
    for expected in 1..=3 {
        {
            let _guard = kspin::NoPreemptIrqSave::new();
            let task = ktask::current();
            let watching = krcu_core::RcuContextIf::current_cpu_watching();
            let cpu = khal::percpu::this_cpu_id();
            let before = (crate::test_support::reader(&task).is_enrolled(), crate::context::needs_reader_cleanup(cpu));
            valid &= crate::read::with_task_read(|token| {
                // SAFETY: watched current-task scope retains the outer borrow.
                let outer = unsafe { value.read(token) };
                // SAFETY: outer guard excludes IRQs and migration; current and
                // container remain live through the nested software NMI.
                let observed = unsafe { read_from_controlled_nmi(&value) };
                observed == (expected, true)
                    && outer.version == expected
                    && crate::test_support::reader(&task).is_reading()
            });
            valid &= !crate::test_support::reader(&task).is_reading() && watching.is_watching();
            // SAFETY: all borrows ended; only balanced NMI work and permanent
            // atomic observations occur before the matching controlled exit.
            unsafe { crate::context::enter_user_quiescent() };
            let was_quiescent = !watching.is_watching();
            // SAFETY: IRQs/migration remain excluded with valid retained current.
            // NMI entry must activate watching before accessing this object.
            let observed = unsafe { read_from_controlled_nmi(&value) };
            let restored_quiescence = !watching.is_watching();
            // SAFETY: matching same-CPU callback after NMI and all borrows ended.
            unsafe { crate::context::exit_user_quiescent() };
            valid &= was_quiescent && restored_quiescence && observed == (expected, true);
            valid &= watching.is_watching() && !crate::test_support::reader(&task).is_reading();
            valid &= before
                == (crate::test_support::reader(&task).is_enrolled(), crate::context::needs_reader_cleanup(cpu));
        }
        if expected < 3 {
            let old = value.replace_sync(payload(expected + 1, &probe)).unwrap();
            returned[expected - 1] = old.version;
            drop(old);
        }
    }
    drop(value);
    assert!(valid);
    assert_eq!(returned, [1, 2]);
    for count in &probe.drops {
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}

#[repr(C)]
struct SoftirqReader {
    node: krcu_core::CallbackNode,
    value: Arc<Rcu<Payload>>,
    reads: Arc<SoftirqReads>,
    cpu: usize,
}

const _: () = core::assert!(core::mem::offset_of!(SoftirqReader, node) == 0);

/// # Safety
/// The node is the zero-offset projection of a Box transferred once to the
/// boot callback service. This invocation consumes that unique allocation.
unsafe fn read_from_softirq(node: *mut krcu_core::CallbackNode) {
    // SAFETY: the checked field offset preserves the submitted Box provenance;
    // the service detached this callback and invokes it exactly once.
    let reader = unsafe { Box::from_raw(node.cast::<SoftirqReader>()) };
    let reads = reader.reads.clone();
    if kirq::context::interrupt_context_level() != kirq::context::InterruptContextLevel::Softirq
        || !krcu_core::RcuContextIf::current_cpu_watching().is_watching()
        || khal::percpu::this_cpu_id().as_usize() != reader.cpu
        || ktask::is_sleepable_context()
    {
        reads.has_invalid_read.store(true, Ordering::Release);
    } else {
        crate::read::with_interrupt_read(|token| {
            // SAFETY: real watched softirq retains current and forbids scheduling.
            // This borrow ends before callback return; reader retains the container.
            let borrowed = unsafe { reader.value.read(token) };
            let version = borrowed.version;
            if !(1..=3).contains(&version)
                || borrowed.probe.drops[version - 1].load(Ordering::Acquire) != 0
            {
                reads.has_invalid_read.store(true, Ordering::Release);
            } else {
                reads.seen.fetch_or(1 << (version - 1), Ordering::Release);
            }
        });
    }
    // Completion must release the captured container first, so the test's final
    // drop remains the last owner and destroys its payload in task context.
    drop(reader);
    reads.completed.fetch_add(1, Ordering::Release);
}

#[def_test(serial)]
fn softirq_reads_publications_during_task_replacement() {
    let service = crate::service::SERVICE.get().unwrap();
    let probe = Arc::new(OwnershipProbe::default());
    let value = Arc::new(Rcu::new(payload(1, &probe)));
    let reads = Arc::new(SoftirqReads::default());
    let submit_fn = |cpu| {
        let reader = Box::into_raw(Box::new(SoftirqReader {
            node: krcu_core::CallbackNode::new(read_from_softirq),
            value: value.clone(),
            reads: reads.clone(),
            cpu,
        }));
        // SAFETY: field projection retains the containing allocation's provenance.
        let node = unsafe { &raw mut (*reader).node };
        // SAFETY: this callback record was never published to readers. The live
        // service owns accepted storage until its nonblocking callback returns.
        if unsafe { service.callbacks.submit(cpu, node) } {
            true
        } else {
            // SAFETY: rejection leaves the original Box exclusively caller-owned.
            drop(unsafe { Box::from_raw(reader) });
            false
        }
    };
    let mut accepted = 0;
    let mut attempted = 0;
    let mut returned = [0; 2];
    let mut replacements_succeeded = true;
    for expected in 1..=3 {
        // First confirm each stable publication is observed. Then leave another
        // batch outstanding while replacing it; overlap is allowed, not forced.
        for cpu in 0..kcpu_id_map::nr_cpus() {
            attempted += 1;
            accepted += usize::from(submit_fn(cpu));
        }
        service.request_pass();
        while reads.completed.load(Ordering::Acquire) < accepted {
            ktask::yield_now();
        }
        if expected < 3 {
            for cpu in 0..kcpu_id_map::nr_cpus() {
                attempted += 1;
                accepted += usize::from(submit_fn(cpu));
            }
            service.request_pass();
            match value.replace_sync(payload(expected + 1, &probe)) {
                Ok(old) => {
                    returned[expected - 1] = old.version;
                    drop(old);
                }
                Err(_) => {
                    replacements_succeeded = false;
                    break;
                }
            }
        }
    }
    // No assertion abandons a callback's container ownership. The bounded QEMU
    // runner diagnoses a stalled service without freeing in-flight storage.
    while reads.completed.load(Ordering::Acquire) < accepted {
        ktask::yield_now();
    }
    drop(value);
    assert!(replacements_succeeded && accepted == attempted);
    assert!(!reads.has_invalid_read.load(Ordering::Acquire));
    assert_eq!(reads.seen.load(Ordering::Acquire), 0b111);
    assert_eq!(returned, [1, 2]);
    for count in &probe.drops {
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}

#[def_test(serial)]
fn timer_irq_reads_each_publication_before_final_reclamation() {
    use ktime_types::TimeSpan;

    let probe = Arc::new(OwnershipProbe::default());
    let value = Arc::new(Rcu::new(payload(1, &probe)));
    let seen = Arc::new(AtomicUsize::new(0));
    let has_invalid_read = Arc::new(AtomicBool::new(false));
    let irq_value = value.clone();
    let irq_probe = probe.clone();
    let irq_seen = seen.clone();
    let irq_invalid = has_invalid_read.clone();
    let timer = ktask::Timer::new(move |control: ktask::TimerControl<'_>, _: ktask::Expiration| {
        // Do not assert or wait inside the handler. A wrong context must be
        // recorded before attempting the unsafe consumer's admission boundary.
        if kirq::context::interrupt_context_level() != kirq::context::InterruptContextLevel::Hardirq
            || !krcu_core::RcuContextIf::current_cpu_watching().is_watching()
            || karch::local_irq_enabled()
        {
            irq_invalid.store(true, Ordering::Release);
            return;
        }
        let observed = crate::read::with_interrupt_read(|token| {
            // SAFETY: real watched timer hardirq retains current and excludes
            // scheduling. The borrowed value ends before this handler returns;
            // the captured Arc keeps its container alive through cancellation.
            let borrowed = unsafe { irq_value.read(token) };
            let version = borrowed.version;
            if !(1..=3).contains(&version)
                || irq_probe.drops[version - 1].load(Ordering::Acquire) != 0
            {
                irq_invalid.store(true, Ordering::Release);
                return 0;
            }
            1 << (version - 1)
        });
        irq_seen.fetch_or(observed, Ordering::Release);
        // Rearm from callback completion, not a potentially expired old deadline.
        if control.rearm(khal::time::monotonic_time() + TimeSpan::from_millis(2)).is_err() {
            irq_invalid.store(true, Ordering::Release);
        }
    });
    // SAFETY: boot initialized the local timer service. The handler only reads
    // within a watched hardirq scope, records atomics and rearms without waiting.
    let started = unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(2)) };
    let mut returned = [0; 2];
    let mut saw_all_versions = started.is_ok();
    if started.is_ok() {
        for expected in 1..=3 {
            let deadline_ns = khal::time::monotonic_time_nanos() + 5_000_000_000;
            while seen.load(Ordering::Acquire) & (1 << (expected - 1)) == 0
                && !has_invalid_read.load(Ordering::Acquire)
                && khal::time::monotonic_time_nanos() < deadline_ns
            {
                ktask::yield_now();
            }
            if seen.load(Ordering::Acquire) & (1 << (expected - 1)) == 0 {
                saw_all_versions = false;
                break;
            }
            if expected < 3 {
                match value.replace_sync(payload(expected + 1, &probe)) {
                    Ok(old) => {
                        returned[expected - 1] = old.version;
                        drop(old);
                    }
                    Err(_) => {
                        saw_all_versions = false;
                        break;
                    }
                }
            }
        }
    }
    // Drain rearming delivery and release its captured container before checking
    // final ownership. Every timeout/error above follows this same cleanup path.
    timer.cancel_sync();
    drop(timer);
    drop(value);
    assert!(started.is_ok() && saw_all_versions);
    assert!(!has_invalid_read.load(Ordering::Acquire));
    assert_eq!(seen.load(Ordering::Acquire), 0b111);
    assert_eq!(returned, [1, 2]);
    for count in &probe.drops {
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}

#[cfg(feature = "smp")]
#[def_test(serial)]
fn migrated_reader_keeps_old_owner_until_scope_exit() {
    // A one-CPU runtime cannot exercise migration, even in an SMP build.
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let probe = Arc::new(OwnershipProbe::default());
    let value = Arc::new(Rcu::new(payload(1, &probe)));
    let has_migrated = Arc::new(AtomicBool::new(false));
    let migration_observed = has_migrated.clone();
    let reader_value = value.clone();
    let reader_probe = probe.clone();
    let source = LogicalCpuId::new(0);
    let destination = LogicalCpuId::new(1);
    let reader = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            crate::read::with_task_read(|token| {
                // SAFETY: this current-task scope protects the borrow across
                // involuntary migration; it neither blocks nor yields.
                let old = unsafe { reader_value.read(token) };
                reader_probe.has_reader_entered.store(true, Ordering::Release);
                while !reader_probe.can_reader_exit.load(Ordering::Acquire) {
                    if khal::percpu::this_cpu_id() == destination {
                        migration_observed.store(true, Ordering::Release);
                    }
                    core::hint::spin_loop();
                }
                reader_probe.is_old_value_intact.store(
                    reader_probe.drops[0].load(Ordering::Acquire) == 0 && old.version == 1,
                    Ordering::Release,
                );
            });
        },
        "rcu-migrating-reader".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    reader.set_cpumask(ktask::KCpuMask::one_shot_logical(source));
    ktask::activate_task(&reader);
    while !probe.has_reader_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    let updater = replace_in_task(&value, 2, &probe);
    while version(&value) != 2 {
        ktask::yield_now();
    }
    let affinity_applied =
        ktask::set_task_affinity(&reader, ktask::KCpuMask::one_shot_logical(destination));
    if affinity_applied {
        // The reader itself confirms execution on CPU1, rather than treating
        // a changed placement field as proof that its stack has resumed there.
        // A bounded QEMU run detects lost progress without a latency assertion.
        while !has_migrated.load(Ordering::Acquire) {
            ktask::yield_now();
        }
    }
    let retains_source = crate::test_support::reader(&reader).is_enrolled_on(crate::context::reader_node(source));
    while updater.state() != ktask::TaskState::Blocked
        && probe.returned[0].load(Ordering::Acquire) == 0
    {
        ktask::yield_now();
    }
    let before_release = probe.returned[0].load(Ordering::Acquire);
    // Release and join even if placement failed, before reporting assertions.
    probe.can_reader_exit.store(true, Ordering::Release);
    let statuses = [reader.join(), updater.join()];
    let is_unlinked = !crate::test_support::reader(&reader).is_enrolled();
    let final_version = version(&value);
    drop(value);
    assert!(affinity_applied);
    assert!(has_migrated.load(Ordering::Acquire));
    assert!(retains_source);
    assert_eq!(before_release, 0);
    assert_eq!(statuses, [0, 0]);
    assert!(is_unlinked);
    assert!(probe.is_old_value_intact.load(Ordering::Acquire));
    assert_eq!(probe.returned[0].load(Ordering::Acquire), 1);
    assert_eq!(final_version, 2);
    assert_eq!(probe.drops[0].load(Ordering::Acquire), 1);
    assert_eq!(probe.drops[1].load(Ordering::Acquire), 1);
    assert!(!probe.has_invalid_drop_context.load(Ordering::Acquire));
}
