// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog initialization and optional NMI handler setup.

use khal::percpu::this_cpu_id;
use ktask::{KCpuMask, TaskInner};
use log::debug;

/// Per-CPU timestamp of the last softlockup report.
/// Rate-limits reports to one per threshold period so a genuine lockup
/// doesn't dump on every timer tick.
#[percpu::def_percpu]
static LAST_SOFTLOCKUP_REPORT: Option<ktime_types::MonotonicInstant> = None;

/// Common watchdog initialization for both primary and secondary CPUs.
///
/// It sets up:
/// - soft lockup detection (timer + watchdog task)
fn init_common() {
    // Hardlockup is "this CPU stopped taking timer IRQs", not "the 4s sample
    // callback did not run between two PMU NMIs". Count every local timer IRQ.
    ktask::register_timer_irq_note(|| {
        crate::timer_tick(khal::time::monotonic_time());
    });
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

    // TODO: read CPU max frequency from DT OPP table (opp-hz).
    // TODO: read CPU max frequency from DT OPP table (opp-hz).
    // For now assume 2.5 GHz.  The hardlockup period is threshold × freq,
    // so over-estimating the cycle frequency stretches the real-time window
    // and makes the detector less sensitive.
    let cpu_freq_hz: u64 = 2_500_000_000;
    // Use u128 intermediate to avoid overflow when threshold × cpu_freq_hz
    // exceeds u64::MAX (e.g. ≥ 1.85 GHz with the 10 s default threshold).
    let nmi_period_cycles = (crate::lockup_detection::DEFAULT_HARDLOCKUP_THRESHOLD.as_nanos()
        * u128::from(cpu_freq_hz)
        / ktime_types::NANOS_PER_SEC as u128) as u64;
    khal::nmi::init(nmi_period_cycles);
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
    // Explicit sample period — Linux softlockup_thresh/5 (4s), independent of
    // the dynamic schedule timer.
    ktask::register_timer_callback(
        core::time::Duration::from_nanos(
            crate::lockup_detection::DEFAULT_WATCHDOG_SAMPLE_PERIOD.as_nanos_u64_saturating(),
        ),
        |_| {
            let now = khal::time::monotonic_time();
            crate::timer_tick(now);

            if crate::check_softlockup(now) {
                // SAFETY: timer callbacks run with interrupts disabled on the
                // current CPU, so per-CPU raw access is safe from migration.
                // SAFETY: timer callbacks run with interrupts disabled and cannot
                // migrate while accessing the current CPU's report timestamp.
                let last = unsafe { *LAST_SOFTLOCKUP_REPORT.current_ref_raw() };
                if last.is_none_or(|last| {
                    now.saturating_duration_since(last)
                        > crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD
                }) {
                    log::error!(
                        "[watchdog] softlockup detected on cpu {}",
                        this_cpu_id().as_usize()
                    );
                    ktask::dump_sched_stats();
                    // SAFETY: timer callback, IRQs disabled, same CPU as the
                    // `read_current_raw` above — cannot race with migration.
                    // SAFETY: the same non-migrating timer callback exclusively
                    // updates this CPU's report timestamp.
                    unsafe { *LAST_SOFTLOCKUP_REPORT.current_ref_mut_raw() = Some(now) };
                    ktask::snapshot::dump_cpu_tasks(this_cpu_id());
                }
            }
        },
    );

    // Sleep 4s between touches instead of yielding — the softlockup threshold
    // is 20s, so this gives 5 wakeup chances (20s / 4s) before a false positive while
    // keeping the CPU truly idle when there is no other work.
    let watchdog_task = TaskInner::new_pidless_kthread(
        move || loop {
            crate::touch_softlockup(khal::time::monotonic_time());
            ktask::sleep(ktime_types::TimeSpan::from_secs(4));
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
