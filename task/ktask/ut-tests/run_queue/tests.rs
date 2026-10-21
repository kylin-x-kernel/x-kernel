// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Run-queue unittests.

#[unittest::mod_test]
mod tests_switch_handoff {
    use alloc::sync::Arc;

    use unittest::{assert_eq, def_test};

    use super::super::PREV_TASK;

    #[cfg(feature = "smp")]
    #[def_test]
    fn switch_cleanup_follows_task_migration() {
        if kcpu_id_map::nr_cpus() < 2 {
            return unittest::TestResult::Ignored;
        }
        let worker = crate::spawn(|| {
            for cpu in (0..2).cycle().take(16) {
                let mut mask = crate::KCpuMask::new();
                mask.set(cpu, true);
                if !crate::set_current_affinity(mask) {
                    crate::exit(1);
                }
                let has_wrong_handoff = {
                    let _irq = kspin::IrqSave::new();
                    khal::percpu::this_cpu_id().as_usize() != cpu
                        || PREV_TASK.shared_current().lock().is_some()
                };
                if has_wrong_handoff {
                    crate::exit(2);
                }
            }
        });
        assert_eq!(worker.join(), 0);
    }

    #[def_test]
    fn incoming_task_consumes_switch_owner_and_exited_task_is_reclaimed() {
        let worker = crate::spawn(|| {
            // Check both the first-run entry and resumed switch continuations.
            for _ in 0..32 {
                let has_previous = {
                    let _irq = kspin::IrqSave::new();
                    PREV_TASK.shared_current().lock().is_some()
                };
                if has_previous {
                    crate::exit(1);
                }
                crate::yield_now();
            }
        });
        let retired = Arc::downgrade(&worker);
        assert_eq!(worker.join(), 0);
        drop(worker);

        // GC runs asynchronously; a leaked switch owner prevents reclamation
        // even though the outgoing task will never resume its continuation.
        while retired.strong_count() != 0 {
            crate::yield_now();
        }
        assert_eq!(retired.strong_count(), 0);
    }
}

#[unittest::mod_test]
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
            // CurrentTask slot + Scheduler.curr + this clone (`strong_count > 2`).
            let keep_alive = current().clone();
            if Arc::strong_count(&keep_alive) > 2 {
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

    /// `resched` must accept idle fallback (`Ready` or still `Running` idle).
    #[def_test]
    fn yield_now_repeatedly_does_not_panic() {
        for _ in 0..8 {
            yield_now();
        }
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
        WakeEnqueue, current_run_queue,
        placement::{
            WakePlacement, can_idle_pull_task, find_idle_pull_src, find_idlest_cpu,
            find_lowest_rt_cpu, select_idle_cpu, select_wake_run_queue, wake_affinity_select,
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
                WakePlacement::RtRank => panic!("fair wake does not use RT rank"),
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

    /// Home outside the mask is an RT rank pick, not the Fair `find_idlest` fallback.
    ///
    /// Keeping home allowed leaves the Fair/Idle runner at rank 0, so
    /// `find_lowest_rt_cpu` stays on the hint and the `RtRank` arm never runs.
    #[def_test]
    fn wake_rt_off_home_is_rt_rank() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let home = khal::percpu::this_cpu_id().as_usize();
            let dest = (home + 1) % kcpu_id_map::nr_cpus();
            let task = test_task_on(home);
            let now_ns = khal::time::monotonic_time_nanos();
            let attr = ksched::SchedAttr {
                policy: ksched::SchedPolicy::Fifo,
                nice: ksched::Nice::DEFAULT,
                rt_priority: ksched::RtPrio::new(1).expect("priority 1 is inside 1..=99"),
                dl_params: None,
            };
            current_run_queue::<NoPreemptIrqSave>()
                .inner
                .with_sched_rank(|s| {
                    s.set_attr(&task, attr, now_ns)
                        .expect("FIFO task is admissible")
                });
            assert_eq!(task.sched_class(), ksched::SchedClass::Rt);

            let mut mask = KCpuMask::new();
            mask.set(dest, true);
            task.set_cpumask(mask);

            let (idx, place) = wake_affinity_select(&task);
            assert_eq!(idx, dest);
            assert_eq!(place, WakePlacement::RtRank);

            #[cfg(feature = "sched_stat")]
            {
                let stats = sched_stat_cpu(LogicalCpuId::new(dest));
                let before = stats.wakeup_rt_rank.load(Ordering::Relaxed);
                drop(select_wake_run_queue::<NoPreemptIrqSave>(&task));
                assert_eq!(stats.wakeup_rt_rank.load(Ordering::Relaxed), before + 1);
            }
        }
    }

    #[def_test]
    fn wake_of_switching_task_defers_enqueue_without_spinning() {
        let task = test_task_on(0);
        task.set_state(TaskState::Blocked);
        task.set_on_cpu(true);

        let (result, decision) = select_wake_run_queue::<NoPreemptIrqSave>(&task)
            .inner
            .put_task_with_state(task.clone(), TaskState::Blocked, false, true);

        assert!(result == WakeEnqueue::Deferred);
        assert!(decision.is_none());
        assert_eq!(task.take_wake_enqueue(), Some((false, true)));
        task.set_on_cpu(false);
    }

    #[def_test]
    fn deferred_wake_targets_recorded_cpu() {
        if kcpu_id_map::nr_cpus() >= 2 {
            let task = test_task_on(0);
            task.set_state(TaskState::Blocked);
            task.set_on_cpu(true);

            let (result, decision) = super::super::get_run_queue(1).put_task_with_state(
                task.clone(),
                TaskState::Blocked,
                false,
                true,
            );

            assert!(result == WakeEnqueue::Deferred);
            assert!(decision.is_none());
            assert_eq!(task.cpu_id().as_usize(), 0);
            assert_eq!(task.take_wake_enqueue(), Some((false, true)));
            assert_eq!(super::super::rq::claim_wake_cpu(&task).as_usize(), 1);
            assert!(task.take_wake_cpu().is_none());
            task.set_on_cpu(false);
        }
    }

    #[def_test]
    fn find_lowest_rt_cpu_prefers_preemptable_hint_then_cpumask_order() {
        let mut mask = KCpuMask::new();
        mask.set(0, true);
        mask.set(1, true);
        // Hint is preemptable, so placement stays there.
        let hint = find_lowest_rt_cpu(&mask, 1, 5, |_| Some(1));
        assert_eq!(hint, 1);
        // Hint is not in the mask. Equal ranks keep the first CPU in mask order.
        let tied = find_lowest_rt_cpu(&mask, 3, 5, |_| Some(2));
        assert_eq!(tied, 0);
    }

    #[cfg(feature = "sched_stat")]
    fn placement_counter(cpu: usize, place: WakePlacement) -> u64 {
        let stats = sched_stat_cpu(LogicalCpuId::new(cpu));
        match place {
            WakePlacement::PrevCpu => stats.wakeup_last_cpu.load(Ordering::Relaxed),
            WakePlacement::IdleSibling => stats.wakeup_idle_sibling.load(Ordering::Relaxed),
            WakePlacement::Fallback => stats.wakeup_fallback.load(Ordering::Relaxed),
            WakePlacement::RtRank => stats.wakeup_rt_rank.load(Ordering::Relaxed),
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
        let task = test_task_on(0);
        let mut mask = KCpuMask::new();
        mask.set(0, true);
        task.set_cpumask(mask);
        assert!(can_idle_pull_task(task.as_ref(), 0));
        task.set_on_cpu(true);
        assert!(!can_idle_pull_task(task.as_ref(), 0));
        task.set_on_cpu(false);
        assert!(can_idle_pull_task(task.as_ref(), 0));
    }

    #[def_test]
    fn can_idle_pull_task_rejects_disallowed_cpu() -> unittest::TestResult {
        if kcpu_id_map::nr_cpus() < 2 {
            return unittest::TestResult::Ignored;
        }
        let task = test_task_on(1);
        let mut only_home = KCpuMask::new();
        only_home.set(1, true);
        task.set_cpumask(only_home);
        assert!(!can_idle_pull_task(task.as_ref(), 0));
        assert!(can_idle_pull_task(task.as_ref(), 1));
        unittest::TestResult::Ok
    }
}

#[unittest::mod_test]
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

#[unittest::mod_test]
mod tests_rt_wire {
    #[cfg(feature = "smp")]
    use khal::percpu::this_cpu_id;
    use ksched::{Nice, RtPrio, SchedAttr, SchedPolicy};
    use kspin::NoPreemptIrqSave;
    use unittest::{assert, assert_eq, def_test};

    use super::super::current_run_queue;
    use crate::{TaskInner, set_task_prio};

    #[def_test]
    fn set_task_prio_keeps_realtime_policy() {
        let task = TaskInner::new_internal(|| {}, "nice-rt".into(), 4096).into_arc();
        #[cfg(feature = "smp")]
        task.set_cpu_id(this_cpu_id());
        let now_ns = khal::time::monotonic_time_nanos();
        let attr = SchedAttr {
            policy: SchedPolicy::Fifo,
            nice: Nice::DEFAULT,
            rt_priority: RtPrio::new(10).expect("priority 10 is inside 1..=99"),
            dl_params: None,
        };
        current_run_queue::<NoPreemptIrqSave>()
            .inner
            .with_sched_rank(|s| {
                s.set_attr(&task, attr, now_ns)
                    .expect("FIFO task is admissible")
            });
        assert!(set_task_prio(&task, 5));
        let got = current_run_queue::<NoPreemptIrqSave>()
            .inner
            .with_sched(|s| s.attr(&task));
        assert_eq!(got.policy, SchedPolicy::Fifo);
        assert_eq!(got.rt_priority.get(), 10);
        assert_eq!(got.nice.as_i32(), 5);
    }
}
