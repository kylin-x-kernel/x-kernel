// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kbuild_config::NR_CPUS;
use kcpu_id_map::LogicalCpuId;
use unittest::{assert, def_test};

use super::MmCpuResidency;

fn fallback_cpu() -> LogicalCpuId {
    LogicalCpuId::new(0)
}

fn alternate_cpu() -> LogicalCpuId {
    let cpu = NR_CPUS.saturating_sub(1);
    LogicalCpuId::new(cpu)
}

#[def_test]
fn test_snapshot_initially_empty() {
    let residency = MmCpuResidency::new();
    assert!(residency.snapshot().is_empty());
}

#[def_test]
fn test_set_and_clear_cpu() {
    let residency = MmCpuResidency::new();
    let cpu = alternate_cpu();
    residency.set_cpu(cpu);
    assert!(residency.snapshot().get(cpu.as_usize()));

    residency.clear_cpu(cpu);
    assert!(!residency.snapshot().get(cpu.as_usize()));
}

#[def_test]
fn test_reset_to_cpu() {
    let residency = MmCpuResidency::new();
    let initial_cpu = fallback_cpu();
    let target_cpu = alternate_cpu();
    residency.set_cpu(initial_cpu);
    residency.set_cpu(target_cpu);

    residency.reset_to_cpu(target_cpu);
    let snapshot = residency.snapshot();
    assert!(snapshot.get(target_cpu.as_usize()));
    if target_cpu != initial_cpu {
        assert!(!snapshot.get(initial_cpu.as_usize()));
    }
}
