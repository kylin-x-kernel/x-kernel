// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real PMU/GIC/IPI lifecycle, run only when no consumer owns a PMU counter.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

static CALLBACKS: [AtomicUsize; kbuild_config::NR_CPUS] =
    [const { AtomicUsize::new(0) }; kbuild_config::NR_CPUS];
static HAS_INVALID_CONTEXT: AtomicBool = AtomicBool::new(false);

fn overflow() {
    if !kirq::context::is_in_nmi() {
        HAS_INVALID_CONTEXT.store(true, Ordering::Relaxed);
    }
    CALLBACKS[khal::percpu::this_cpu_id().as_usize()].fetch_add(1, Ordering::Relaxed);
}

fn on_cpu(cpu: usize, body: impl FnOnce() -> bool + Send + 'static) -> bool {
    let result = Arc::new(AtomicBool::new(false));
    let completed = result.clone();
    let sent = kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
        completed.store(body(), Ordering::Release);
    });
    sent.unwrap();
    result.load(Ordering::Acquire)
}

#[def_test(serial)]
fn real_pmu_global_shutdown_retries_and_republishes_after_reader_drain() -> unittest::TestResult {
    if kplat::nm_irq::mode() == kplat::nm_irq::NmiMode::None {
        return unittest::TestResult::Ignored;
    }
    // The UT runner starts after boot CPU services. No hotplug or system stop
    // runs here. Do not steal live watchdog/perf ownership in normal configs.
    let cpus = if cfg!(feature = "smp") {
        kcpu_id_map::nr_cpus()
    } else {
        1
    };
    for cpu in 0..cpus {
        if !on_cpu(cpu, || {
            let pin = kspin::NoPreemptIrqSave::new();
            super::super::PMU
                .get(&pin)
                .get()
                .is_none_or(|pmu| pmu.counters.iter().all(Option::is_none))
        }) {
            return unittest::TestResult::Ignored;
        }
    }
    HAS_INVALID_CONTEXT.store(false, Ordering::Relaxed);
    for round in 0..2 {
        for (cpu, calls) in CALLBACKS.iter().enumerate().take(cpus) {
            calls.store(0, Ordering::Relaxed);
            assert!(on_cpu(cpu, || kplat::nm_irq::enable_periodic_nmi(
                100_000, overflow
            )));
        }
        // Callback acknowledgement, not elapsed wall time, proves real delivery.
        // The external QEMU timeout diagnoses a missing hardware interrupt.
        while CALLBACKS[..cpus]
            .iter()
            .any(|calls| calls.load(Ordering::Relaxed) == 0)
        {
            core::hint::spin_loop();
        }
        if round == 0 {
            let result = retry_partial_shutdown(cpus);
            if !result.is_ok() {
                return result;
            }
        }
        assert_eq!(khal::pmu::shutdown_overflow_irq(), Ok(()));
        assert!(!HAS_INVALID_CONTEXT.load(Ordering::Relaxed));
        for cpu in 0..cpus {
            assert!(on_cpu(cpu, || {
                let pin = kspin::NoPreemptIrqSave::new();
                super::super::PMU.get(&pin).get().is_some_and(|pmu| {
                    pmu.counters.iter().all(Option::is_none)
                        && pmu.overflow_handlers.iter().all(Option::is_none)
                })
            }));
        }
        assert_eq!(khal::pmu::restore_overflow_irq(), Ok(()));
        assert!(super::can_start());
        unittest::ktest_println!(
            "PMU global shutdown/republication round {} completed on {} CPUs",
            round,
            cpus
        );
    }
    // Restore boot state: registered/enabled PPIs, no consumer callbacks.
    for cpu in 0..cpus {
        assert!(on_cpu(cpu, || {
            khal::pmu::enable_irq();
            true
        }));
    }
    unittest::TestResult::Ok
}

fn retry_partial_shutdown(cpus: usize) -> unittest::TestResult {
    let failed_cpu = cpus - 1;
    let mut attempted = [false; kbuild_config::NR_CPUS];
    assert_eq!(
        super::shutdown_with(|cpu| {
            attempted[cpu] = true;
            if cpu == failed_cpu {
                Err(KError::ResourceBusy)
            } else {
                super::stop_cpu(cpu)
            }
        }),
        Err(KError::ResourceBusy)
    );
    assert!(!super::can_start());
    assert!(khal::pmu::restore_overflow_irq().is_err());
    for (cpu, did_attempt) in attempted.iter().enumerate().take(failed_cpu) {
        assert!(*did_attempt);
        assert!(on_cpu(cpu, || !kplat::nm_irq::enable_periodic_nmi(
            100_000, overflow
        )));
    }
    // Completed CPUs must not acquire another nested disable on retry.
    let mut retried = [false; kbuild_config::NR_CPUS];
    assert_eq!(
        super::shutdown_with(|cpu| {
            retried[cpu] = true;
            super::stop_cpu(cpu)
        }),
        Ok(())
    );
    assert!(retried[failed_cpu]);
    assert!(retried[..failed_cpu].iter().all(|did_retry| !did_retry));
    unittest::TestResult::Ok
}

#[def_test]
fn global_pmu_management_rejects_irq_exclusion_before_owner_lock() {
    let _pin = kspin::NoPreemptIrqSave::new();
    assert_eq!(
        khal::pmu::shutdown_overflow_irq(),
        Err(KError::InvalidInput)
    );
    assert_eq!(khal::pmu::restore_overflow_irq(), Err(KError::InvalidInput));
}
