// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, def_test};

use super::*;

#[def_test]
fn snapshots_keep_sparse_cpu_slots_independent_and_unidentified_cpus_absent() {
    // Local slots leave the permanent boot snapshots untouched.
    let slots: [Once<CpuInfo>; 3] = [const { Once::new() }; 3];
    let boot = *cpu_info(LogicalCpuId::new(0)).unwrap();
    slots[0].call_once(|| boot);
    slots[2].call_once(|| boot);
    let first = lookup_cpu_info(&slots, LogicalCpuId::new(0)).unwrap();
    let third = lookup_cpu_info(&slots, LogicalCpuId::new(2)).unwrap();
    assert!(!core::ptr::eq(first, third));
    assert!(lookup_cpu_info(&slots, LogicalCpuId::new(1)).is_none());
    assert!(lookup_cpu_info(&slots, LogicalCpuId::new(3)).is_none());
    assert!(lookup_cpu_info(&slots, LogicalCpuId::new(usize::MAX)).is_none());
}
