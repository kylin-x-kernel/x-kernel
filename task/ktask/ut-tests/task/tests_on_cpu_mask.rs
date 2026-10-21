// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn new_test_task() -> TaskInner {
    TaskInner::new_common(TaskIdentity::Internal, "test".into())
}

#[def_test]
fn test_on_cpu_mask_initially_empty() {
    let task = new_test_task();
    assert!(task.on_cpu_mask().is_empty());
}

#[def_test]
fn test_set_on_cpu_mask_bit() {
    if kcpu_id_map::nr_cpus() >= 2 {
        let task = new_test_task();
        task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
        assert!(task.on_cpu_mask().get(0));
        assert!(!task.on_cpu_mask().get(1));
    }
}

#[def_test]
fn test_reset_on_cpu_mask() {
    if kcpu_id_map::nr_cpus() >= 3 {
        let task = new_test_task();
        task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
        task.set_on_cpu_mask_bit(LogicalCpuId::new(1));
        task.reset_on_cpu_mask(LogicalCpuId::new(2));
        let mask = task.on_cpu_mask();
        assert!(!mask.get(0));
        assert!(!mask.get(1));
        assert!(mask.get(2));
    }
}

#[def_test]
fn test_on_cpu_mask_multiple_bits() {
    if kcpu_id_map::nr_cpus() >= 4 {
        let task = new_test_task();
        task.set_on_cpu_mask_bit(LogicalCpuId::new(0));
        task.set_on_cpu_mask_bit(LogicalCpuId::new(2));
        task.set_on_cpu_mask_bit(LogicalCpuId::new(3));
        let mask = task.on_cpu_mask();
        assert_eq!(mask.len(), 3);
        assert!(mask.get(0));
        assert!(!mask.get(1));
        assert!(mask.get(2));
        assert!(mask.get(3));
    }
}
