// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use unittest::{assert, assert_eq, assert_ne, def_test};

use crate::{IPI_QUEUE_READY, KipiError};

#[def_test]
fn test_error_display_messages() {
    assert_eq!(
        alloc::format!("{}", KipiError::InvalidCpuId),
        "Invalid CPU ID"
    );
    assert_eq!(
        alloc::format!("{}", KipiError::TargetCpuNotReady),
        "Target CPU is not ready for IPI"
    );
    assert_eq!(alloc::format!("{}", KipiError::QueueFull), "IPI queue full");
    assert_eq!(
        alloc::format!("{}", KipiError::NoMemory),
        "IPI callback allocation failed"
    );
    assert_eq!(
        alloc::format!("{}", KipiError::CallbackFailed),
        "Callback execution failed"
    );
}

#[def_test]
fn test_error_equality() {
    assert_ne!(KipiError::InvalidCpuId, KipiError::CallbackFailed);
}

#[def_test]
fn local_callback_does_not_prepare_an_owner() {
    let calls = alloc::sync::Arc::new(AtomicUsize::new(0));
    let hits = calls.clone();
    let prepared = core::cell::Cell::new(false);
    let result = {
        let _guard = kspin::NoPreemptIrqSave::new();
        crate::run_on_cpu_with_prepare(
            khal::percpu::this_cpu_id(),
            move || {
                hits.fetch_add(1, Ordering::Relaxed);
            },
            |_| {
                prepared.set(true);
                Err(kerrno::KError::NoMemory)
            },
        )
    };
    assert_eq!(result, Ok(()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(!prepared.get());
}

struct CallbackDropProbe(alloc::sync::Arc<AtomicUsize>);

impl Drop for CallbackDropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn remote_owner_failure_drops_capture_before_publication() {
    let calls = alloc::sync::Arc::new(AtomicUsize::new(0));
    let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
    let prepared = core::cell::Cell::new(false);
    let result = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let current_cpu = khal::percpu::this_cpu_id();
        let mut remote = None;
        for_each_present_logical_cpu(|_, cpu, _| {
            if cpu != current_cpu && remote.is_none() {
                remote = Some(cpu);
            }
        });
        let Some(remote) = remote else {
            return unittest::TestResult::Ignored;
        };
        let hits = calls.clone();
        let probe = CallbackDropProbe(drops.clone());
        crate::run_on_cpu_with_prepare(
            remote,
            move || {
                hits.fetch_add(1, Ordering::Relaxed);
                drop(probe);
            },
            |callback| {
                prepared.set(true);
                drop(callback);
                Err(kerrno::KError::NoMemory)
            },
        )
    };
    assert_eq!(result, Err(KipiError::NoMemory));
    assert!(prepared.get());
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn invalid_target_drops_prepared_owner_without_invocation() {
    let calls = alloc::sync::Arc::new(AtomicUsize::new(0));
    let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
    let hits = calls.clone();
    let probe = CallbackDropProbe(drops.clone());
    let callback = crate::Callback::new(move || {
        hits.fetch_add(1, Ordering::Relaxed);
        drop(probe);
    })
    .unwrap();
    assert_eq!(
        crate::run_on_cpu_prepared(LogicalCpuId::new(kbuild_config::NR_CPUS), callback),
        Err(KipiError::InvalidCpuId),
    );
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn prepared_callback_can_execute_locally_with_irqs_disabled() {
    let calls = alloc::sync::Arc::new(AtomicUsize::new(0));
    let hits = calls.clone();
    let callback = crate::Callback::new(move || {
        hits.fetch_add(1, Ordering::Relaxed);
    })
    .unwrap();
    let result = {
        let _guard = kspin::NoPreemptIrqSave::new();
        crate::run_on_cpu_prepared(khal::percpu::this_cpu_id(), callback)
    };
    assert_eq!(result, Ok(()));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn prepared_consuming_callback_runs_on_remote_cpu_in_hardirq() {
    let completed = alloc::sync::Arc::new(AtomicUsize::new(0));
    let drops = alloc::sync::Arc::new(AtomicUsize::new(0));
    let observed_cpu = alloc::sync::Arc::new(AtomicUsize::new(usize::MAX));
    let done = completed.clone();
    let cpu_seen = observed_cpu.clone();
    let probe = CallbackDropProbe(drops.clone());
    let callback = crate::Callback::new(move || {
        cpu_seen.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
        let context = if kirq::context::is_in_hardirq() { 1 } else { 2 };
        drop(probe);
        done.store(context, Ordering::Release);
    })
    .unwrap();
    let pin = kspin::NoPreempt::new();
    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote = None;
    for_each_present_logical_cpu(|_, cpu, _| {
        if cpu != current_cpu && remote.is_none() {
            remote = Some(cpu);
        }
    });
    let Some(remote) = remote else {
        return unittest::TestResult::Ignored;
    };
    crate::run_on_cpu_prepared(remote, callback).unwrap();
    let start_ns = khal::time::monotonic_time_nanos();
    while completed.load(Ordering::Acquire) == 0 {
        if khal::time::monotonic_time_nanos().saturating_sub(start_ns) >= 5_000_000_000 {
            break;
        }
        core::hint::spin_loop();
    }
    let context = completed.load(Ordering::Acquire);
    drop(pin);
    assert_eq!(context, 1);
    assert_eq!(observed_cpu.load(Ordering::Relaxed), remote.as_usize());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn test_error_debug_format() {
    let text = alloc::format!("{:?}", KipiError::InvalidCpuId);
    assert!(text.contains("InvalidCpuId"));
}

#[def_test]
fn test_run_on_cpu_rejects_non_present_cpu() {
    let mut non_present_cpu = None;
    for logical_cpu_id in 0..kbuild_config::NR_CPUS {
        let logical_cpu_id = LogicalCpuId::new(logical_cpu_id);
        if raw_cpu_id(logical_cpu_id).is_none() {
            non_present_cpu = Some(logical_cpu_id);
            break;
        }
    }

    if let Some(cpu_id) = non_present_cpu {
        assert_eq!(
            crate::run_on_cpu(cpu_id, || {}),
            Err(KipiError::InvalidCpuId)
        );
    }
}

#[def_test(serial)]
fn test_run_on_cpu_rejects_not_ready_present_cpu() {
    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        IPI_QUEUE_READY[cpu_id.as_usize()].store(false, Ordering::Release);
        let result = crate::run_on_cpu(cpu_id, || {});
        IPI_QUEUE_READY[cpu_id.as_usize()].store(true, Ordering::Release);
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));
    }
}

#[def_test(serial)]
fn test_run_on_each_cpu_does_not_execute_locally_when_remote_not_ready() {
    static HIT: AtomicUsize = AtomicUsize::new(0);

    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        IPI_QUEUE_READY[cpu_id.as_usize()].store(false, Ordering::Release);
        HIT.store(0, Ordering::SeqCst);
        let result = crate::run_on_each_cpu(|| {
            HIT.fetch_add(1, Ordering::SeqCst);
        });
        IPI_QUEUE_READY[cpu_id.as_usize()].store(true, Ordering::Release);
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));
        assert_eq!(HIT.load(Ordering::SeqCst), 0);
    }
}

#[def_test(serial)]
fn test_run_on_each_cpu_via_ipi_failure_does_not_leave_local_event() {
    static HIT: AtomicUsize = AtomicUsize::new(0);

    let current_cpu = khal::percpu::this_cpu_id();
    let mut remote_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu && remote_cpu.is_none() {
            remote_cpu = Some(cpu_id);
        }
    });

    if let Some(cpu_id) = remote_cpu {
        IPI_QUEUE_READY[cpu_id.as_usize()].store(false, Ordering::Release);
        HIT.store(0, Ordering::SeqCst);
        let result = crate::run_on_each_cpu_via_ipi(|| {
            HIT.fetch_add(1, Ordering::SeqCst);
        });
        IPI_QUEUE_READY[cpu_id.as_usize()].store(true, Ordering::Release);
        assert_eq!(result, Err(KipiError::TargetCpuNotReady));

        crate::ipi_handler();
        assert_eq!(HIT.load(Ordering::SeqCst), 0);
    }
}
