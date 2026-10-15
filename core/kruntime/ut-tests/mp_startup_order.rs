// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId, for_each_present_logical_cpu};
use unittest::{assert, assert_eq, def_test};

// One-shot boot observations intentionally live for the whole test boot.
// No runtime hooks are installed, and tasks own no references to stack data.
static COMPLETE: [AtomicBool; kbuild_config::NR_CPUS] =
    [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS];
static RAN_EARLY: AtomicBool = AtomicBool::new(false);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
static REMAINING: AtomicUsize = AtomicUsize::new(0);
static SERVICES_READY_BEFORE_RCU: AtomicBool = AtomicBool::new(false);
static PRIMARY_WAITING_FOR_SERVICES: AtomicBool = AtomicBool::new(false);

pub(super) fn mark_complete(cpu_id: LogicalCpuId) {
    // Hold AP publication until the primary actually enters the service wait.
    // This boot-only handshake needs no scheduled task or timer to release it.
    while !PRIMARY_WAITING_FOR_SERVICES.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    COMPLETE[cpu_id.as_usize()].store(true, Ordering::Release);
}

pub(crate) fn record_service_wait() {
    PRIMARY_WAITING_FOR_SERVICES.store(true, Ordering::Release);
}

pub(crate) fn record_service_barrier(primary: LogicalCpuId) {
    let mut all_complete = true;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != primary {
            all_complete &= COMPLETE[cpu_id.as_usize()].load(Ordering::Acquire);
        }
    });
    SERVICES_READY_BEFORE_RCU.store(all_complete, Ordering::Release);
}

pub(crate) fn queue_early_tasks(primary: LogicalCpuId) {
    // All AP run queues exist, but APs still wait for the primary's
    // INITED_CPUS publication. No task can pass the startup checkpoint yet.
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id == primary {
            return;
        }
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                if !COMPLETE[cpu_id.as_usize()].load(Ordering::Acquire) {
                    RAN_EARLY.store(true, Ordering::Release);
                }
                REMAINING.fetch_sub(1, Ordering::AcqRel);
            },
            "startup_order_probe".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu_id));
        QUEUED.fetch_add(1, Ordering::Relaxed);
        REMAINING.fetch_add(1, Ordering::Relaxed);
        ktask::activate_task(&task);
        kipi::notify_reschedule(cpu_id).expect("boot probe reschedule request failed");
    });
}

#[def_test]
fn cpu_services_are_ready_before_rcu_initialization() {
    assert!(SERVICES_READY_BEFORE_RCU.load(Ordering::Acquire));
    if kcpu_id_map::nr_cpus() > 1 {
        assert!(PRIMARY_WAITING_FOR_SERVICES.load(Ordering::Acquire));
    }
}

#[def_test]
fn early_remote_task_waits_for_secondary_bootstrap() {
    // Wait for explicit task completion, not a wall-clock delay or a fixed
    // number of yields. The external QEMU timeout bounds a boot failure.
    while REMAINING.load(Ordering::Acquire) != 0 {
        ktask::yield_now();
    }
    assert_eq!(QUEUED.load(Ordering::Acquire), kcpu_id_map::nr_cpus() - 1);
    assert!(!RAN_EARLY.load(Ordering::Acquire));
}
