// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(not(test))]
use unittest::{assert, assert_eq};

use super::{tests::quiescent, *};
use crate::{PeriodId, ReaderNode, TaskReader, grace_period::GracePeriodCoordinator};

fn observe<const CPUS: usize>(
    gp: &GracePeriodCoordinator<'_, CPUS>,
    cpu: usize,
    watching: &CpuWatching,
) -> bool {
    // SAFETY: these tests exclusively drive isolated CPU/node fixtures. No real
    // data is retired; observations and transitions model that same CPU.
    unsafe { gp.observe_watching(cpu, watching) }
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn currently_quiescent_cpu_can_report_without_a_scheduler_switch() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let cpu = CpuWatching::new();
    assert!(!observe(&gp, 0, &cpu));
    let observed = quiescent(&cpu, || {
        let started = gp.begin_period();
        let reported = observe(&gp, 0, &cpu);
        (started, reported, gp.try_complete_period())
    });
    assert_eq!(observed, (Some(1), true, Some(1)));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn active_interrupt_blocks_quiescence_until_its_exit() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let cpu = CpuWatching::new();
    let observed = quiescent(&cpu, || {
        // SAFETY: isolated CPU fixture; the entry is exited before returning
        // to the enclosing quiescent window and cannot migrate or schedule.
        let entry = unsafe { cpu.enter_interrupt() };
        let started = gp.begin_period();
        let during = (observe(&gp, 0, &cpu), gp.try_complete_period());
        // SAFETY: no nested entries remain and all simulated accesses finished.
        unsafe { entry.exit() };
        let after = (observe(&gp, 0, &cpu), gp.try_complete_period());
        (started, during, after)
    });
    assert_eq!(observed, (Some(1), (false, None), (true, Some(1))));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn watched_samples_require_a_crossing_after_this_periods_snapshot() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let cpu = CpuWatching::new();
    // A crossing predating the GP is not evidence for that GP.
    quiescent(&cpu, || {});
    assert_eq!(gp.begin_period(), Some(1));
    assert!(!observe(&gp, 0, &cpu));
    assert!(!observe(&gp, 0, &cpu));
    assert_eq!(gp.try_complete_period(), None);
    quiescent(&cpu, || {});
    assert!(cpu.is_watching());
    assert!(observe(&gp, 0, &cpu));
    assert!(!observe(&gp, 0, &cpu));
    assert_eq!(gp.try_complete_period(), Some(1));
    assert_eq!(gp.begin_period(), Some(2));
    assert!(!observe(&gp, 0, &cpu));
    assert!(nodes[0].needs_cpu_observation());
    assert_eq!(gp.try_complete_period(), None);
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn quiescent_cpu_does_not_release_its_preempted_task_reader() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let cpu = CpuWatching::new();
    let reader = TaskReader::new();
    let observed = reader.with_read(|_| {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated retained reader, one simulated scheduler under
        // exclusion. Enrollment models a switch away from the active reader.
        unsafe { nodes[0].enroll(&reader) };
        let started = gp.begin_period();
        quiescent(&cpu, || {
            let reported = observe(&gp, 0, &cpu);
            (
                started,
                reported,
                nodes[0].needs_cpu_observation(),
                reader.is_enrolled(),
                gp.try_complete_period(),
            )
        })
    });
    assert_eq!(observed, (Some(1), false, false, true, None));
    assert!(gp.report_node(0));
    assert_eq!(gp.try_complete_period(), Some(1));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn full_watching_wrap_alias_does_not_certify_quiescence() {
    let nodes = [ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true]).unwrap();
    let cpu = CpuWatching::new();
    assert_eq!(gp.begin_period(), Some(1));
    assert!(!observe(&gp, 0, &cpu));
    // Simulate the end of an entire counter cycle without doing usize::MAX
    // transitions. Equal odd samples remain ambiguous, so must not report.
    cpu.sequence.store(1, Ordering::Release);
    assert!(!observe(&gp, 0, &cpu));
    assert_eq!(gp.try_complete_period(), None);
    quiescent(&cpu, || {});
    assert!(observe(&gp, 0, &cpu));
    assert_eq!(gp.try_complete_period(), Some(1));
}

#[cfg_attr(test, test)]
#[cfg_attr(not(test), unittest::def_test)]
fn period_wrap_and_absent_cpus_do_not_reuse_observations() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    let gp = GracePeriodCoordinator::new(&nodes, [true, false]).unwrap();
    let cpu = CpuWatching::new();
    let absent_cpu = CpuWatching::new();
    for count in 1..=(usize::from(PeriodId::MAX) + 2) {
        let expected = count as PeriodId;
        assert_eq!(gp.begin_period(), Some(expected));
        assert!(!observe(&gp, 1, &absent_cpu));
        assert!(!observe(&gp, 2, &absent_cpu));
        assert!(!observe(&gp, 0, &cpu));
        assert_eq!(gp.try_complete_period(), None);
        quiescent(&cpu, || {});
        assert!(observe(&gp, 0, &cpu));
        assert_eq!(gp.try_complete_period(), Some(expected));
    }
}
