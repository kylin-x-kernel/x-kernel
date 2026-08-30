// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Watchdog initialization and optional NMI handler setup.

use alloc::format;

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use ktime_types::TimeSpan;
use log::{debug, warn};

/// Per-CPU timestamp of the last softlockup report.
/// Rate-limits reports to one per threshold period so a genuine lockup
/// doesn't dump on every timer tick.
#[percpu::def_percpu]
static LAST_SOFTLOCKUP_REPORT: Option<ktime_types::MonotonicInstant> = None;

const SOFTLOCKUP_TOUCH_INTERVAL: TimeSpan = TimeSpan::from_secs(4);
const WATCHDOG_TASK_PRIO: isize = -20;

/// Common watchdog initialization for both primary and secondary CPUs.
///
/// It sets up:
/// - soft lockup detection (timer check + per-CPU watchdog task)
fn init_common() {
    // Hardlockup is "this CPU stopped taking timer IRQs", not "the 4s sample
    // callback did not run between two PMU NMIs". Count every local timer IRQ.
    ktask::register_timer_irq_note(|| {
        crate::timer_tick(khal::time::monotonic_time());
    });
    init_softlockup_detection();

    // Register mutex deadlock check (checked from NMI context only).
    #[cfg(feature = "nmi")]
    crate::register_watchdog_task(&crate::watchdog_task::MUTEX_DEADLOCK_CHECK);
    crate::register_watchdog_task(&SYSTEM_WORKQUEUE_CHECK);

    #[cfg(feature = "nmi")]
    init_nmi_watchdog();

    debug!("watchdog init success on cpu {}", this_cpu_id().as_usize());
}

#[cfg(feature = "nmi")]
fn init_nmi_watchdog() {
    use crate::rendezvous as rv;

    // Degraded build: the compiled-in NMI mechanism is not supported on this
    // hardware, so there is nothing to arm.  `enable_periodic_nmi` would
    // fail the same way; report it once instead of attempting.
    if khal::nmi::mode() == khal::nmi::NmiMode::None {
        log::warn!("[watchdog] NMI mechanism unavailable; hard lockup detection disabled");
        return;
    }

    // Register hard lockup detection task.
    crate::register_hardlockup_detection_task();

    // Start periodic NMI delivery through the source‑neutral NMI interface.
    // The platform backend (currently the PMU cycle counter) internally:
    //   1. Computes the cycle threshold from period_ns.
    //   2. Initialises the per-CPU counter hardware.
    //   3. Registers our per-CPU callback.
    //   4. Promotes the PMU line to NMI delivery on this CPU (PPI).
    //   5. Starts the counter.
    // Steps 2-4 are fallible; the counter and callback are set up *before*
    // the line is promoted, so any failure is rolled back completely by
    // `deinit_cycle_counter()` and promotion is the last fallible step.
    // The PMU overflow-dispatch handler on the interrupt line is registered
    // independently by the `pmu` feature init (kruntime), so perf overflow
    // delivery works even without the hardlockup watchdog.
    if !khal::nmi::enable_periodic_nmi(
        crate::lockup_detection::DEFAULT_HARDLOCKUP_THRESHOLD.as_nanos() as u64,
        || {
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
                        "[watchdog] failure detected on cpu {}, failed_task={:?}, \
                         arrived_mask={:#x}",
                        this_cpu,
                        fail_name,
                        rv::arrived_bitmap()
                    );

                    // Cause CPU dumps all tasks for all CPUs.
                    ktask::snapshot::nmi_dump_all(rv::all_arrived_mask());

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
        },
    ) {
        log::error!(
            "[watchdog] failed to arm periodic NMI on cpu {}; hard lockup detection disabled",
            this_cpu_id().as_usize()
        );
    }
}

/// Initialize soft lockup detection.
///
/// A per-CPU watchdog task periodically updates a timestamp, and timer
/// callbacks check whether the timestamp is stale.
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
                    // SAFETY: the same non-migrating timer callback exclusively
                    // updates this CPU's report timestamp.
                    unsafe { *LAST_SOFTLOCKUP_REPORT.current_ref_mut_raw() = Some(now) };
                    ktask::snapshot::dump_cpu_tasks(this_cpu_id());
                }
            }
        },
    );

    let cpu_id = this_cpu_id();
    crate::touch_softlockup(khal::time::monotonic_time());
    start_softlockup_watchdog_task(cpu_id);
}

fn start_softlockup_watchdog_task(cpu_id: LogicalCpuId) {
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        softlockup_watchdog_task,
        format!("watchdog/{}", cpu_id.as_usize()),
        kbuild_config::TASK_STACK_SIZE,
    ));
    // Spawn pin: write the mask, then activate onto a CPU in that mask
    // (same as ksoftirqd). `set_task_affinity` migrates a task that already
    // occupies a runqueue.
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu_id));
    ktask::activate_task(&task);
    if !ktask::set_task_prio(&task, WATCHDOG_TASK_PRIO) {
        warn!(
            "watchdog: failed to raise softlockup watchdog task priority on cpu {}",
            cpu_id.as_usize()
        );
    }
}

fn softlockup_watchdog_task() {
    loop {
        crate::touch_softlockup(khal::time::monotonic_time());
        ktask::sleep(SOFTLOCKUP_TOUCH_INTERVAL);
    }
}

struct SystemWorkqueueCheck;

static SYSTEM_WORKQUEUE_CHECK: SystemWorkqueueCheck = SystemWorkqueueCheck;

impl crate::watchdog_task::WatchdogTask for SystemWorkqueueCheck {
    fn name(&self) -> &str {
        "SystemWorkqueue"
    }

    fn check(&self) -> bool {
        kwork::raw::system_workqueue_watchdog_check(
            khal::time::monotonic_time(),
            crate::lockup_detection::DEFAULT_SOFTLOCKUP_THRESHOLD,
        )
    }
}

/// Initialize watchdogs on the primary CPU.
pub fn init_primary() {
    init_common();
}

/// Initialize watchdogs on a secondary CPU.
pub fn init_secondary() {
    init_common();
}
