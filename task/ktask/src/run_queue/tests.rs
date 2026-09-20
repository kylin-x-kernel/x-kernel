// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Run-queue unittests.

#[cfg(unittest)]
mod tests_blocked_resched {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kspin::{NoPreemptIrqSave, SpinNoIrq};
    use unittest::{assert_eq, def_test};

    use super::super::{current_run_queue, select_wake_run_queue};
    use crate::{current, spawn, yield_now};

    /// Direct `blocked_resched` path (not via `block_on`) with an explicit
    /// caller-owned strong ref — the contract any second caller must follow.
    #[def_test(serial)]
    fn blocked_resched_survives_with_caller_owned_ref() {
        static STARTED: AtomicUsize = AtomicUsize::new(0);
        static RESUMED: AtomicUsize = AtomicUsize::new(0);
        // `unittest::assert!` returns `TestResult`; cannot use it inside spawn.
        static STRONG_OK: AtomicUsize = AtomicUsize::new(0);

        STARTED.store(0, Ordering::Release);
        RESUMED.store(0, Ordering::Release);
        STRONG_OK.store(0, Ordering::Release);

        let waiter = spawn(move || {
            // into_raw current slot = 1; this clone is the required second.
            let keep_alive = current().clone();
            if Arc::strong_count(&keep_alive) > 1 {
                STRONG_OK.store(1, Ordering::Release);
            }

            let woke = SpinNoIrq::new(false);
            {
                let mut rq = current_run_queue::<NoPreemptIrqSave>();
                STARTED.store(1, Ordering::Release);
                rq.blocked_resched(woke.lock());
            }
            drop(keep_alive);
            RESUMED.store(1, Ordering::Release);
        });

        while STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        // Wake may race the Running→Blocked transition; retry until resume.
        while RESUMED.load(Ordering::Acquire) == 0 {
            select_wake_run_queue::<NoPreemptIrqSave>(&waiter).unblock_task(waiter.clone(), true);
            yield_now();
        }
        assert_eq!(STRONG_OK.load(Ordering::Acquire), 1);
        assert_eq!(RESUMED.load(Ordering::Acquire), 1);
    }
}

#[cfg(all(feature = "smp", unittest))]
mod tests_wake_affinity {
    #[cfg(feature = "sched_stat")]
    use core::sync::atomic::Ordering;

    use kcpu_id_map::LogicalCpuId;
    use kspin::NoPreemptIrqSave;
    use unittest::{assert, assert_eq, def_test};

    #[cfg(feature = "sched_stat")]
    use super::super::stats::sched_stat_cpu;
    use super::super::{
        WakeEnqueue,
        placement::{
            WakePlacement, can_idle_pull_task, find_idle_pull_src, find_idlest_cpu,
            select_idle_cpu, select_wake_run_queue, wake_affinity_select,
        },
    };
    use crate::{KCpuMask, KtaskRef, TaskInner, TaskState};

    fn test_task_on(home: usize) -> KtaskRef {
        let task = TaskInner::new_internal(|| {}, "wake-aff".into(), 4096).into_arc();
        task.set_cpu_id(LogicalCpuId::new(home));
        task
    }

    /// `prepare_task` leaves `Ready` + `cpu_id == 0` and no RQ occupancy.
    /// Affinity must be mask-only there; spinning until enqueue never arrives.
    #[def_test]
    fn set_task_affinity_on_unpublished_task_is_mask_only() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = TaskInner::new_internal(|| {}, "aff-prep".into(), 4096).into_arc();
            assert_eq!(task.cpu_id().as_usize(), 0);
            assert!(task.is_ready());

            let mut only1 = KCpuMask::new();
            only1.set(1, true);
            assert!(crate::set_task_affinity(&task, only1));
            assert!(task.cpumask().get(1));
            assert!(!task.cpumask().get(0));
            assert_eq!(task.state(), TaskState::Ready);
        }
    }

    #[def_test]
    fn select_idle_cpu_keeps_prev_when_idle() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = select_idle_cpu(&mask, Some(1), |cpu| cpu == 1);
            assert_eq!(idx, Some(1));
        }
    }

    #[def_test]
    fn select_idle_cpu_seeks_sibling_when_prev_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = select_idle_cpu(&mask, Some(1), |cpu| cpu == 0);
            assert_eq!(idx, Some(0));
        }
    }

    #[def_test]
    fn select_idle_cpu_none_when_all_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            assert_eq!(select_idle_cpu(&mask, Some(1), |_| false), None);
        }
    }

    #[def_test]
    fn wake_affinity_stays_home_when_prev_idle() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            task.set_cpumask(mask);

            // CPU1 is typically idle in this unittest; if the test itself runs
            // there, SIS may pick another idle CPU — still a legal wake.
            let (idx, place) = wake_affinity_select(&task);
            match place {
                WakePlacement::PrevCpu => assert_eq!(idx, 1),
                WakePlacement::IdleSibling => assert!(idx != 1 && task.cpumask().get(idx)),
                WakePlacement::Fallback => panic!("home is still in cpumask"),
            }
        }
    }

    #[def_test]
    fn wake_affinity_falls_back_when_home_forbidden() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            task.set_cpumask(mask);

            let (idx, place) = wake_affinity_select(&task);
            assert_eq!(idx, 0);
            assert!(matches!(
                place,
                WakePlacement::IdleSibling | WakePlacement::Fallback
            ));
        }
    }

    #[def_test]
    fn wake_of_switching_task_defers_enqueue_without_spinning() {
        let task = test_task_on(0);
        task.set_state(TaskState::Blocked);
        task.set_on_cpu(true);

        let result = select_wake_run_queue::<NoPreemptIrqSave>(&task)
            .inner
            .put_task_with_state(task.clone(), TaskState::Blocked, false, true);

        assert!(result == WakeEnqueue::Deferred);
        assert_eq!(task.take_wake_enqueue(), Some((false, true)));
        task.set_on_cpu(false);
    }

    #[cfg(feature = "sched_stat")]
    fn placement_counter(cpu: usize, place: WakePlacement) -> u64 {
        let stats = sched_stat_cpu(LogicalCpuId::new(cpu));
        match place {
            WakePlacement::PrevCpu => stats.wakeup_last_cpu.load(Ordering::Relaxed),
            WakePlacement::IdleSibling => stats.wakeup_idle_sibling.load(Ordering::Relaxed),
            WakePlacement::Fallback => stats.wakeup_fallback.load(Ordering::Relaxed),
        }
    }

    #[cfg(feature = "sched_stat")]
    #[def_test]
    fn select_wake_run_queue_counts_placement() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(1);
            let mut all = KCpuMask::new();
            all.set(0, true);
            all.set(1, true);
            task.set_cpumask(all);

            let (idx, place) = wake_affinity_select(&task);
            let before = placement_counter(idx, place);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            assert_eq!(placement_counter(idx, place), before + 1);

            let mut only0 = KCpuMask::new();
            only0.set(0, true);
            task.set_cpumask(only0);
            let (idx0, place0) = wake_affinity_select(&task);
            assert_eq!(idx0, 0);
            let before0 = placement_counter(idx0, place0);
            {
                let _rq = select_wake_run_queue::<NoPreemptIrqSave>(&task);
            }
            assert_eq!(placement_counter(idx0, place0), before0 + 1);
        }
    }

    #[def_test]
    fn find_idlest_prefers_lighter_cpu() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, None, |cpu| match cpu {
                0 => Some(3),
                1 => Some(1),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idlest_round_robins_ties() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let first = find_idlest_cpu(&mask, 0, None, |_| Some(0));
            let second = find_idlest_cpu(&mask, 1, None, |_| Some(0));
            assert_eq!(first, 0);
            assert_eq!(second, 1);
        }
    }

    #[def_test]
    fn find_idlest_prefers_local_on_tie() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(1), |_| Some(1));
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idlest_ignores_heavier_prefer() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some(2),
                1 => Some(1),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    /// Idle-first: a sleeper's CPU (`is_busy == false`) ranks ahead of a
    /// running CPU. Key is `(is_busy, nr_home)` as in [`super::spawn_idlest_key`].
    #[def_test]
    fn find_idlest_idle_beats_busy() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some((true, 1usize)),
                1 => Some((false, 1usize)),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    /// Among idle CPUs, a vacant CPU ranks ahead of a sleeper's CPU.
    #[def_test]
    fn find_idlest_vacant_idle_beats_sleeper() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let mut mask = KCpuMask::new();
            mask.set(0, true);
            mask.set(1, true);
            let idx = find_idlest_cpu(&mask, 0, Some(0), |cpu| match cpu {
                0 => Some((false, 1usize)),
                1 => Some((false, 0usize)),
                _ => None,
            });
            assert_eq!(idx, 1);
        }
    }

    #[def_test]
    fn find_idle_pull_src_picks_busiest() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let tried = KCpuMask::new();
            let src = find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                0 => 4,
                1 => 3,
                _ => 0,
            });
            assert_eq!(src, Some(1));
        }
    }

    #[def_test]
    fn find_idle_pull_src_ignores_lone_and_tried() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let tried = KCpuMask::new();
            assert_eq!(
                find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                    1 => 1,
                    _ => 0,
                }),
                None
            );

            let mut tried = KCpuMask::new();
            tried.set(1, true);
            assert_eq!(
                find_idle_pull_src(0, &tried, 2, |cpu| match cpu {
                    1 => 5,
                    _ => 0,
                }),
                None
            );
        }
    }

    #[def_test]
    fn can_idle_pull_task_rejects_on_cpu() {
        let task = test_task_on(1);
        let mut mask = KCpuMask::new();
        mask.set(0, true);
        mask.set(1, true);
        task.set_cpumask(mask);
        assert!(can_idle_pull_task(&task, 0));
        task.set_on_cpu(true);
        assert!(!can_idle_pull_task(&task, 0));
        task.set_on_cpu(false);
        let mut only_home = KCpuMask::new();
        only_home.set(1, true);
        task.set_cpumask(only_home);
        assert!(!can_idle_pull_task(&task, 0));
    }
}

#[cfg(all(feature = "preempt", unittest))]
mod tests_interrupt_kick {
    use unittest::{assert, def_test};

    use crate::current;

    #[def_test(serial)]
    fn interrupt_current_sets_preempt_pending() {
        let curr = current();
        curr.disable_preempt();
        curr.set_preempt_pending(false);
        curr.clear_interrupt();
        curr.interrupt();
        assert!(curr.is_interrupted());
        assert!(curr.is_preempt_pending());
        curr.clear_interrupt();
        curr.enable_preempt(false);
    }

    #[cfg(feature = "smp")]
    #[def_test(serial)]
    fn interrupt_running_remote_task_sets_preempt_pending() {
        use core::sync::atomic::{AtomicUsize, Ordering};

        use kcpu_id_map::LogicalCpuId;
        use khal::percpu::this_cpu_id;
        use unittest::assert_eq;

        use crate::{KCpuMask, set_task_affinity, spawn, yield_now};

        const TIMEOUT_NS: u64 = 2_000_000_000;

        static READY: AtomicUsize = AtomicUsize::new(0);
        static DONE: AtomicUsize = AtomicUsize::new(0);
        static FAILED: AtomicUsize = AtomicUsize::new(0);
        static STOP: AtomicUsize = AtomicUsize::new(0);

        if kcpu_id_map::nr_cpus() >= 2 {
            READY.store(0, Ordering::Release);
            DONE.store(0, Ordering::Release);
            FAILED.store(0, Ordering::Release);
            STOP.store(0, Ordering::Release);

            let local = this_cpu_id().as_usize();
            let remote = (0..kcpu_id_map::nr_cpus())
                .find(|&cpu| cpu != local)
                .expect("smp unittest must expose a second CPU");

            let task = spawn(move || {
                let mut mask = KCpuMask::new();
                mask.set(remote, true);
                if !set_task_affinity(&current().clone(), mask) {
                    FAILED.store(1, Ordering::Release);
                    DONE.store(1, Ordering::Release);
                    return;
                }

                while current().cpu_id().as_usize() != remote && STOP.load(Ordering::Acquire) == 0 {
                    yield_now();
                }

                let curr = current();
                curr.disable_preempt();
                curr.set_preempt_pending(false);
                curr.clear_interrupt();
                READY.store(1, Ordering::Release);

                let start = khal::time::monotonic_time_nanos();
                while STOP.load(Ordering::Acquire) == 0 {
                    if curr.is_interrupted() && curr.is_preempt_pending() {
                        break;
                    }
                    if khal::time::monotonic_time_nanos().saturating_sub(start) > TIMEOUT_NS {
                        FAILED.store(1, Ordering::Release);
                        break;
                    }
                    core::hint::spin_loop();
                }
                curr.enable_preempt(false);
                DONE.store(1, Ordering::Release);
            });

            let start = khal::time::monotonic_time_nanos();
            while READY.load(Ordering::Acquire) == 0 {
                if khal::time::monotonic_time_nanos().saturating_sub(start) > TIMEOUT_NS {
                    break;
                }
                yield_now();
            }
            assert_eq!(READY.load(Ordering::Acquire), 1);
            assert!(task.on_cpu());
            assert_eq!(task.cpu_id(), LogicalCpuId::new(remote));

            task.interrupt();

            while DONE.load(Ordering::Acquire) == 0 {
                if khal::time::monotonic_time_nanos().saturating_sub(start) > TIMEOUT_NS {
                    break;
                }
                yield_now();
            }
            STOP.store(1, Ordering::Release);
            assert_eq!(DONE.load(Ordering::Acquire), 1);
            assert_eq!(FAILED.load(Ordering::Acquire), 0);
        }
    }
}
