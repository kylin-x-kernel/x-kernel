// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog initialization and optional NMI handler setup.

use khal::percpu::this_cpu_id;
use ktask::{KCpuMask, TaskInner};
use log::debug;

/// Common watchdog initialization for both primary and secondary CPUs.
///
/// It sets up:
/// - soft lockup detection (timer + watchdog task)
fn init_common() {
    init_softlockup_detection();

    // Register mutex deadlock check
    crate::register_watchdog_task(&crate::watchdog_task::MUTEX_DEADLOCK_CHECK);

    #[cfg(feature = "nmi")]
    init_nmi_watchdog();

    debug!("watchdog init success on cpu {}", this_cpu_id().as_usize());
}

#[cfg(feature = "nmi")]
fn init_nmi_watchdog() {
    use crate::rendezvous as rv;

    // Register hard lockup detection task.
    crate::register_hardlockup_detection_task();

    // Initialize and enable NMI source for hard lockup detection.
    khal::nmi::init(khal::time::freq() * 10 * 16);
    khal::nmi::enable();

    // Register NMI handler
    khal::nmi::register_nmi_handler(|| {
        // Every NMI checks whether watchdog tasks on THIS CPU are healthy.
        // If a failure is detected, THIS CPU becomes the cause CPU and
        // triggers a global rendezvous.
        let fail_name = crate::watchdog_task::check_watchdog_tasks();
        if fail_name.is_some() {
            if ktask::snapshot::nmi_begin() {
                rv::try_trigger();
            } else {
                khal::kprint_atomic!("[watchdog] snapshot already running, skip NMI dump\n");
            }
        }

        // Once any CPU triggered, ALL CPUs must rendezvous here.
        if rv::is_triggered() {
            rv::mark_arrived();
            ktask::snapshot::nmi_collect_local();
            let this_cpu = this_cpu_id().as_usize();
            let is_cause = rv::cause_cpu() == Some(this_cpu);
            if is_cause {
                // Strong rendezvous: MUST wait until all CPUs are in NMI.
                rv::wait_all_arrived_strong();

                khal::kprint_atomic!(
                    "[watchdog] failure detected on cpu {}, failed_task={:?}, arrived_mask={:#x}",
                    this_cpu,
                    fail_name,
                    rv::arrived_bitmap()
                );

                // Cause CPU dumps all tasks for all CPUs.
                ktask::snapshot::nmi_dump_all(rv::all_arrived_mask(), true);

                // Notify others that dump is done.
                rv::mark_dump_done();
                ktask::snapshot::nmi_finish();

                // Hard stop on the cause CPU.
                panic!("Watchdog task check failed (global dump)");
            } else {
                // Non-cause CPUs: spin until dump is done.
                while !rv::is_dump_done() {
                    core::hint::spin_loop();
                }
            }
        }
    });
}

/// Initialize soft lockup detection.
///
/// A per-CPU watchdog task periodically updates a timestamp,
/// and timer callbacks check whether the timestamp is stale.
/// Initialize soft lockup detection on the current CPU.
pub fn init_softlockup_detection() {
    // Timer callback used to detect soft lockup conditions.
    ktask::register_timer_callback(|_| {
        let now_ns = khal::time::monotonic_time_nanos();
        crate::timer_tick();

        if crate::check_softlockup(now_ns) {
            ktask::snapshot::dump_cpu_tasks(this_cpu_id());
        }
    });

    // Watchdog task that periodically "touches" the soft lockup timestamp.
    let watchdog_task = TaskInner::new(
        move || loop {
            crate::touch_softlockup(khal::time::monotonic_time_nanos());
            ktask::yield_now();
        },
        "watchdog".into(),
        kbuild_config::TASK_STACK_SIZE,
    );

    // Bind watchdog task to the local CPU.
    watchdog_task.set_cpumask(KCpuMask::one_shot(this_cpu_id().as_usize()));
    ktask::spawn_task(watchdog_task);
}

/// Initialize watchdogs on the primary CPU.
pub fn init_primary() {
    init_common();
}

/// Initialize watchdogs on a secondary CPU.
pub fn init_secondary() {
    init_common();
}
