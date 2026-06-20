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
        alloc::format!("{}", KipiError::CallbackFailed),
        "Callback execution failed"
    );
}

#[def_test]
fn test_error_equality() {
    assert_ne!(KipiError::InvalidCpuId, KipiError::CallbackFailed);
}

#[def_test]
fn test_error_debug_format() {
    let text = alloc::format!("{:?}", KipiError::InvalidCpuId);
    assert!(text.contains("InvalidCpuId"));
}

#[def_test]
fn test_run_on_cpu_rejects_non_present_cpu() {
    let mut non_present_cpu = None;
    for logical_cpu_id in 0..kbuild_config::CPU_NUM {
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

#[def_test]
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

#[def_test]
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

#[def_test]
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
