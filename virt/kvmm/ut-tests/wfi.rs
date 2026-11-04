// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Exercise actual WFI parking and owned deadline retirement without guest code.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kiface_task_api::{ScheduleOutReason, TaskScheduleObserver};

use super::*;
use crate::vm::{Vm, VmConfig};

struct Blocked {
    phase: AtomicUsize,
    observed: AtomicUsize,
}

// SAFETY: both callbacks are bounded and only access a retained atomic counter.
unsafe impl TaskScheduleObserver for Blocked {
    unsafe fn schedule_out(&self, _: kcpu_id_map::LogicalCpuId, reason: ScheduleOutReason) {
        if reason == ScheduleOutReason::Blocked {
            self.observed
                .store(self.phase.load(Ordering::Acquire), Ordering::Release);
        }
    }

    unsafe fn schedule_in(&self, _: kcpu_id_map::LogicalCpuId) {}
}

#[unittest::def_test(serial)]
fn disabled_and_masked_wfi_park_repeatedly_and_observe_stop() {
    const WAITS: usize = 16;
    let vm = Vm::<Aarch64Vhe>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    let shared = vm.shared().clone();
    let blocked = Arc::new(Blocked {
        phase: AtomicUsize::new(0),
        observed: AtomicUsize::new(0),
    });
    let worker = blocked.clone();
    let task = ktask::spawn_task(
        ktask::TaskInner::new_pidless_kthread(
            move || {
                vcpu.vm.set_vcpu_task(0, ktask::current().clone());
                // Neither control permits timer output. Observe actual parking
                // before each wake; this does not distinguish an unbounded wait
                // from a bounded polling sleep that also parks.
                for i in 0..WAITS {
                    vcpu.arch.cntv_ctl = if i & 1 == 0 { 0 } else { 3 };
                    worker.phase.store(i + 1, Ordering::Release);
                    core::assert!(matches!(handle_wfi(&mut vcpu, 0), ExitAction::Resume));
                    core::assert_eq!(worker.observed.load(Ordering::Acquire), i + 1);
                }
                core::assert!(vcpu.vm.is_stop_requested());
            },
            "vcpu-wfi-reblock".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .with_schedule_observer(blocked.clone()),
    );
    for expected in 1..=WAITS {
        // One future can park repeatedly. Do not mistake an extra park in the
        // old phase for readiness to send the next phase's interrupt.
        while blocked.observed.load(Ordering::Acquire) < expected {
            ktask::yield_now();
        }
        if expected == WAITS {
            shared.request_stop();
        } else {
            ktask::interrupt_task(&task, true);
        }
    }
    assert_eq!(task.join(), 0);
    assert_eq!(blocked.observed.load(Ordering::Acquire), WAITS);
}

struct ExpiryRace {
    is_blocked: AtomicBool,
    has_entered: AtomicBool,
    is_released: AtomicBool,
    has_finished: AtomicBool,
    is_dropping: AtomicBool,
    has_returned: AtomicBool,
    phase: AtomicUsize,
    observed: AtomicUsize,
}

impl ExpiryRace {
    fn new() -> Self {
        Self {
            is_blocked: AtomicBool::new(false),
            has_entered: AtomicBool::new(false),
            is_released: AtomicBool::new(false),
            has_finished: AtomicBool::new(false),
            is_dropping: AtomicBool::new(false),
            has_returned: AtomicBool::new(false),
            phase: AtomicUsize::new(1),
            observed: AtomicUsize::new(0),
        }
    }
}

// SAFETY: scheduling observers only publish atomic fixture state. No wait,
// allocation, task lock, or reentry into the scheduler occurs here.
unsafe impl TaskScheduleObserver for ExpiryRace {
    unsafe fn schedule_out(&self, _: kcpu_id_map::LogicalCpuId, reason: ScheduleOutReason) {
        if reason == ScheduleOutReason::Blocked {
            self.observed
                .store(self.phase.load(Ordering::Acquire), Ordering::Release);
            self.is_blocked.store(true, Ordering::Release);
        }
    }

    unsafe fn schedule_in(&self, _: kcpu_id_map::LogicalCpuId) {
        self.is_blocked.store(false, Ordering::Release);
    }
}

impl ktask::future::SleepProbeForTests for ExpiryRace {
    fn before_wake(&self) {
        // If expiry wins before the first park, let it finish and retry a new
        // wait. The test never assumes the host can park within a time budget.
        if !self.is_blocked.load(Ordering::Acquire) {
            return;
        }
        self.has_entered.store(true, Ordering::Release);
        while !self.is_released.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }

    fn after_wake(&self) {
        if self.has_entered.load(Ordering::Acquire) {
            self.has_finished.store(true, Ordering::Release);
        }
    }

    fn on_cancel_wait(&self) {
        if self.has_entered.load(Ordering::Acquire) {
            self.is_dropping.store(true, Ordering::Release);
        }
    }
}

fn run_expiry_race(stop_during_expiry: bool) {
    use aarch64_cpu::registers::{CNTFRQ_EL0, CNTPCT_EL0, CNTVOFF_EL2, Readable};
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    core::assert!(ktask::set_current_affinity(
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(2))
    ));
    let vm = Vm::<Aarch64Vhe>::new(VmConfig::new(0, 0, 1)).expect("test VM");
    let mut vcpu = vm.create_vcpu(0);
    let shared = vm.shared().clone();
    let race = Arc::new(ExpiryRace::new());
    let worker = race.clone();
    let task = ktask::prepare_task(
        ktask::TaskInner::new_pidless_kthread(
            move || {
                vcpu.vm.set_vcpu_task(0, ktask::current().clone());
                ktask::future::with_sleep_probe_for_tests(worker.clone(), || {
                    loop {
                        {
                            let _irq = kspin::IrqSave::new();
                            vcpu.arch.cntv_ctl = 1;
                            vcpu.arch.cntv_cval = CNTPCT_EL0.get().wrapping_sub(CNTVOFF_EL2.get())
                                + CNTFRQ_EL0.get() / 500;
                        }
                        core::assert!(matches!(handle_wfi(&mut vcpu, 0), ExitAction::Resume));
                        if worker.has_entered.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    core::assert!(worker.has_finished.load(Ordering::Acquire));
                    core::assert_eq!(khal::percpu::this_cpu_id(), LogicalCpuId::new(1));
                    worker.has_returned.store(true, Ordering::Release);
                });
                if !stop_during_expiry {
                    // The expired operation wins over the interrupt hint. Clear
                    // that explicit hint before checking the next wait's lifetime.
                    ktask::current().clear_interrupt();
                    vcpu.arch.cntv_ctl = 0;
                    worker.phase.store(2, Ordering::Release);
                    core::assert!(matches!(handle_wfi(&mut vcpu, 0), ExitAction::Resume));
                }
                core::assert!(vcpu.vm.is_stop_requested());
            },
            "vcpu-wfi-expiry-race".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .with_schedule_observer(race.clone()),
    );
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    ktask::activate_task(&task);
    while !race.has_entered.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    // CPU0 is inside the callback, CPU1 can retire its owned timer, and CPU2
    // controls release. Never depend on progress from the IRQ-blocked CPU0.
    core::assert!(ktask::set_task_affinity(
        &task,
        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(1))
    ));
    if stop_during_expiry {
        shared.request_stop();
    } else {
        ktask::interrupt_task(&task, true);
    }
    while !race.is_dropping.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    core::assert!(!race.has_returned.load(Ordering::Acquire));
    race.is_released.store(true, Ordering::Release);
    if !stop_during_expiry {
        while race.observed.load(Ordering::Acquire) != 2 {
            ktask::yield_now();
        }
        shared.request_stop();
    }
    core::assert_eq!(task.join(), 0);
    core::assert!(race.has_returned.load(Ordering::Acquire));
    core::assert!(race.has_finished.load(Ordering::Acquire));
}

#[unittest::def_test(serial)]
fn wfi_stop_and_reblock_drain_remote_inflight_expiry() {
    if !cfg!(feature = "smp") || kcpu_id_map::nr_cpus() < 3 {
        return unittest::TestResult::Ignored;
    }
    for stop_during_expiry in [false, true] {
        let task = ktask::spawn_task(ktask::TaskInner::new_pidless_kthread(
            move || run_expiry_race(stop_during_expiry),
            "vcpu-expiry-controller".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        assert_eq!(task.join(), 0);
    }
}
