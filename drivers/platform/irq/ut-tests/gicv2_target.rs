// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use arm_gic_driver::v2::{CpuInterfaceTarget, CpuTargetMap, TargetList};
use kcpu_id_map::{LogicalCpuId, RawCpuId};
use unittest::{assert_eq, def_test};

use super::{GicInitError, record_cpu_target, target_for_logical_cpu};

#[def_test]
fn missing_hardware_identity_has_no_recorded_target() {
    let routes = CpuTargetMap::new();
    assert_eq!(
        target_for_logical_cpu(LogicalCpuId::new(3), &routes, |_| None),
        None
    );
}

#[def_test]
fn implicit_target_is_not_reachable_from_another_logical_cpu() {
    let routes = CpuTargetMap::new();
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(0),
            0,
            CpuInterfaceTarget::ImplicitUniprocessor,
        ),
        Ok(())
    );
    assert_eq!(
        target_for_logical_cpu(LogicalCpuId::new(0), &routes, |_| Some(RawCpuId::new(0))),
        Some(CpuInterfaceTarget::ImplicitUniprocessor)
    );
    assert_eq!(
        target_for_logical_cpu(LogicalCpuId::new(1), &routes, |_| Some(RawCpuId::new(1))),
        None
    );
}

#[def_test]
fn duplicate_association_is_idempotent_and_conflicts_never_overwrite() {
    let routes = CpuTargetMap::new();
    let target = CpuInterfaceTarget::Explicit(TargetList::from_raw(0x20));
    assert_eq!(
        record_cpu_target(&routes, LogicalCpuId::new(1), 0x105, target),
        Ok(())
    );
    assert_eq!(
        record_cpu_target(&routes, LogicalCpuId::new(1), 0x105, target),
        Ok(())
    );
    assert_eq!(
        routes.cpu_interface_target_for_logical_cpu(1),
        Some(target)
    );
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(1),
            0x105,
            CpuInterfaceTarget::Explicit(TargetList::from_raw(0x40)),
        ),
        Err(GicInitError::ConflictingCpuTarget)
    );
    assert_eq!(
        record_cpu_target(&routes, LogicalCpuId::new(2), 0x105, target),
        Err(GicInitError::ConflictingCpuTarget)
    );
    assert_eq!(
        record_cpu_target(&routes, LogicalCpuId::new(2), 0x106, target),
        Err(GicInitError::ConflictingCpuTarget)
    );
    assert_eq!(routes.cpu_interface_target_for_logical_cpu(1), Some(target));

    let implicit_routes = CpuTargetMap::new();
    assert_eq!(
        record_cpu_target(
            &implicit_routes,
            LogicalCpuId::new(0),
            0,
            CpuInterfaceTarget::ImplicitUniprocessor,
        ),
        Ok(())
    );
    assert_eq!(
        record_cpu_target(
            &implicit_routes,
            LogicalCpuId::new(0),
            0,
            CpuInterfaceTarget::ImplicitUniprocessor,
        ),
        Ok(())
    );
    assert_eq!(
        record_cpu_target(
            &implicit_routes,
            LogicalCpuId::new(1),
            1,
            CpuInterfaceTarget::ImplicitUniprocessor,
        ),
        Err(GicInitError::InvalidCpuTarget)
    );
}

#[def_test]
fn out_of_range_logical_cpu_and_non_one_hot_targets_are_rejected() {
    let routes = CpuTargetMap::new();
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(8),
            0x108,
            CpuInterfaceTarget::Explicit(TargetList::from_raw(0x20)),
        ),
        Err(GicInitError::LogicalCpuOutOfRange)
    );
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(0),
            0,
            CpuInterfaceTarget::Explicit(TargetList::from_raw(0)),
        ),
        Err(GicInitError::InvalidCpuTarget)
    );
    assert_eq!(
        record_cpu_target(
            &routes,
            LogicalCpuId::new(0),
            0,
            CpuInterfaceTarget::Explicit(TargetList::from_raw(0x50)),
        ),
        Err(GicInitError::InvalidCpuTarget)
    );
}
