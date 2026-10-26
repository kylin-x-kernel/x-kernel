// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Verify local and remote lookup without mutating live watching state.

use unittest::{assert, assert_eq, def_test};

#[def_test]
fn current_and_remote_slots_name_the_same_permanent_state() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let cpu = kiface_cpu_api::CurrentCpuIf::current_cpu();
    let area = kcpu_slot::area(cpu).unwrap();
    let local = super::current_state();
    let remote = super::CPU_STATE.at(&area);
    assert!(core::ptr::eq(local, remote));
    assert!(core::ptr::eq(&local.watching, super::cpu_watching(&area)));
    assert_eq!(super::needs_local_reader_cleanup(), super::needs_reader_cleanup(cpu));

    for other in 0..kcpu_id_map::nr_cpus() {
        let other_area = kcpu_slot::area(kcpu_id_map::LogicalCpuId::new(other)).unwrap();
        let other_state = super::CPU_STATE.at(&other_area);
        let distance = (core::ptr::from_ref(local) as usize)
            .abs_diff(core::ptr::from_ref(other_state) as usize);
        assert_eq!(distance, cpu.as_usize().abs_diff(other) * kcpu_slot::area_size());
    }
}
