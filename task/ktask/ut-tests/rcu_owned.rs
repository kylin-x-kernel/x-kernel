// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Controlled task-only consumer. The boot RCU service persists for this run.

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
    let current = crate::current();
    let observed = current.rcu_reader.with_read(|token| {
        // SAFETY: this is the current task's scope and the boot service is live.
        *unsafe { value.read(token) }
    });
    assert_eq!(observed, 11);
}

#[def_test]
fn replacement_inside_read_scope_leaves_both_owners_unchanged() {
    let value = Rcu::new(Box::new(11_u64));
    let current = crate::current();
    let (result, observed) = current.rcu_reader.with_read(|token| {
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
        if !crate::is_sleepable_context() {
            self.probe.has_invalid_drop_context.store(true, Ordering::Release);
        }
        self.probe.drops[self.version - 1].fetch_add(1, Ordering::Release);
    }
}

fn payload(version: usize, probe: &Arc<OwnershipProbe>) -> Box<Payload> {
    Box::new(Payload { version, probe: probe.clone() })
}

fn version(value: &Rcu<Payload>) -> usize {
    let current = crate::current();
    current.rcu_reader.with_read(|token| {
        // SAFETY: this live container is read only through the current task's
        // scope in kernel execution; its data does not escape the token.
        unsafe { value.read(token) }.version
    })
}

fn replace_in_task(value: &Arc<Rcu<Payload>>, next: usize, probe: &Arc<OwnershipProbe>) -> crate::KtaskRef {
    let value = value.clone();
    let probe = probe.clone();
    let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let old = value.replace_sync(payload(next, &probe)).unwrap();
            probe.returned[next - 2].store(old.version, Ordering::Release);
            drop(old);
        },
        "rcu-replace".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    crate::activate_task(&task);
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
    let reader = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let current = crate::current();
            current.rcu_reader.with_read(|token| {
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
    reader.set_cpumask(crate::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    crate::activate_task(&reader);
    while !probe.has_reader_entered.load(Ordering::Acquire) {
        crate::yield_now();
    }
    let first = replace_in_task(&value, 2, &probe);
    while version(&value) != 2 {
        crate::yield_now();
    }
    let second = replace_in_task(&value, 3, &probe);
    while version(&value) != 3 {
        crate::yield_now();
    }
    // An otherwise uncontended CPU need not switch on a reschedule request.
    // Joining a real same-CPU competitor establishes a scheduler boundary
    // before the enrollment check. The reader cannot exit its scope until we
    // release it below, so registration cannot disappear before that check.
    let competitor = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        || {},
        "rcu-reader-competitor".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    competitor.set_cpumask(crate::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    crate::activate_task(&competitor);
    let competitor_status = competitor.join();
    let was_enrolled = reader.rcu_reader.is_enrolled();
    // Each updater must reach its wait, or publish an erroneous early return.
    // Do not mistake an updater that has not run yet for a correctly blocked one.
    for (index, updater) in [&first, &second].into_iter().enumerate() {
        while updater.state() != crate::TaskState::Blocked
            && probe.returned[index].load(Ordering::Acquire) == 0
        {
            crate::yield_now();
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
    let reader = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            let current = crate::current();
            current.rcu_reader.with_read(|token| {
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
    reader.set_cpumask(crate::KCpuMask::one_shot_logical(source));
    crate::activate_task(&reader);
    while !probe.has_reader_entered.load(Ordering::Acquire) {
        crate::yield_now();
    }
    let updater = replace_in_task(&value, 2, &probe);
    while version(&value) != 2 {
        crate::yield_now();
    }
    let affinity_applied =
        crate::set_task_affinity(&reader, crate::KCpuMask::one_shot_logical(destination));
    if affinity_applied {
        // The reader itself confirms execution on CPU1, rather than treating
        // a changed placement field as proof that its stack has resumed there.
        // A bounded QEMU run detects lost progress without a latency assertion.
        while !has_migrated.load(Ordering::Acquire) {
            crate::yield_now();
        }
    }
    let retains_source = reader.rcu_reader.is_enrolled_on(crate::rcu::reader_node(source));
    while updater.state() != crate::TaskState::Blocked
        && probe.returned[0].load(Ordering::Acquire) == 0
    {
        crate::yield_now();
    }
    let before_release = probe.returned[0].load(Ordering::Acquire);
    // Release and join even if placement failed, before reporting assertions.
    probe.can_reader_exit.store(true, Ordering::Release);
    let statuses = [reader.join(), updater.join()];
    let is_unlinked = !reader.rcu_reader.is_enrolled();
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
