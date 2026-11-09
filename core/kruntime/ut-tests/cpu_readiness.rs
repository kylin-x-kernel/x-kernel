// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

use super::InterruptReadiness;

#[def_test]
fn only_explicitly_published_cpu_identities_are_ready() {
    let readiness = InterruptReadiness::new();
    assert_eq!(readiness.snapshot().iter_logical().count(), 0);

    // The last configured slot need not be present or booted in this fixture:
    // publication tracks identity, not a count or a contiguous online prefix.
    let cpu = LogicalCpuId::new(kbuild_config::NR_CPUS - 1);
    readiness.publish(cpu);
    let mask = readiness.snapshot();
    assert!(mask.get_logical(cpu));
    assert_eq!(mask.iter_logical().count(), 1);

    readiness.publish(cpu);
    assert_eq!(readiness.snapshot(), mask);
}

#[def_test]
fn boot_readiness_matches_started_cpus_not_configured_capacity() {
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let mut expected = kcpu_id_map::KCpuMask::new();
    #[cfg(feature = "smp")]
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
        expected.set_logical(cpu, true);
    });
    #[cfg(not(feature = "smp"))]
    expected.set_logical(khal::percpu::this_cpu_id(), true);
    assert_eq!(ready, expected);
}
