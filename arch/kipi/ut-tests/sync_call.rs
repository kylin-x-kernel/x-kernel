// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use unittest::{assert_eq, def_test};

#[def_test]
fn invalid_target_does_not_invoke_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let result = super::run_on_cpu_sync(LogicalCpuId::new(kbuild_config::NR_CPUS), move || {
        count.fetch_add(1, Ordering::Release);
    });
    assert_eq!(result, Err(kerrno::KError::InvalidInput));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}

#[def_test]
fn masked_caller_is_rejected_before_execution() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let result = {
        let _irq = kspin::IrqSave::new();
        super::run_on_cpu_sync(khal::percpu::this_cpu_id(), move || {
            count.fetch_add(1, Ordering::Release);
        })
    };
    assert_eq!(result, Err(kerrno::KError::InvalidInput));
    assert_eq!(calls.load(Ordering::Acquire), 0);
}
