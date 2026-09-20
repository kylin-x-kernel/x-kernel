// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task placement and completion driver for the standalone timer core's races.

use alloc::boxed::Box;

use kcpu_id_map::KCpuMaskExt;
use unittest::def_test;

#[def_test]
fn concurrent_idle_restarts_and_cancels_keep_route_consistent() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let result =
        ktimer_core::test_support::concurrent_owner_routes(run_two_workers, crate::yield_now);
    unittest::assert_eq!(result, unittest::TestResult::Ok);
}

#[def_test]
fn remote_cancel_waits_for_callback_cleanup_and_removes_rearm() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let result = ktimer_core::test_support::concurrent_cancel_waits_for_cleanup(
        run_two_workers,
        crate::yield_now,
    );
    unittest::assert_eq!(result, unittest::TestResult::Ok);
}

fn run_two_workers(workers: [Box<dyn FnOnce() + Send>; 2]) -> bool {
    let mut cpu = 0;
    let tasks = workers.map(|worker| {
        let selected = cpu;
        cpu += 1;
        let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
            move || {
                let is_expected_cpu = khal::percpu::this_cpu_id().as_usize() == selected;
                worker();
                crate::exit(if is_expected_cpu { 0 } else { 1 });
            },
            "timer_race_probe".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(crate::KCpuMask::one_shot_logical(selected.into()));
        task
    });
    for task in &tasks {
        crate::activate_task(task);
    }
    let mut succeeded = true;
    for task in &tasks {
        succeeded &= task.join() == 0;
    }
    succeeded
}
