// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Flat, equal-capacity Fair balancing from the scheduler softirq.
//!
//! Tick/IPI entries only request work. The softirq needs a brought-up local
//! scheduler, not a process thread; it never sleeps or allocates. Source and
//! destination RQ locks never overlap. Each individual RQ hold masks IRQs,
//! since the softirq action itself runs with IRQs enabled.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use kcpu_id_map::LogicalCpuId;
use kirq::softirq::{SoftirqVec, raise_softirq_irqoff};
use ksched::{FairBalanceStats, PeltAvg};
use kspin::NoPreemptIrqSave;

use super::{
    KCpuMask, RUN_QUEUES, RunQueue, current_run_queue_ref, migrate::migrate_entry,
    placement::can_idle_pull_task,
};

const MIN_INTERVAL_NS: u64 = 16_000_000;
const MAX_INTERVAL_NS: u64 = 128_000_000;
const MIGRATION_COOLDOWN_NS: u64 = 32_000_000;
const MAX_CPUS_PER_SCAN: usize = 32;
const MAX_SOURCE_ATTEMPTS: usize = 2;
const MAX_TASKS_PER_SCAN: usize = 32;

kcpu_slot::cpu_slot_cell! {
    static NEXT_BALANCE_NS: u64 = 0;
}
kcpu_slot::cpu_slot_cell! {
    static BALANCE_INTERVAL_NS: u64 = crate::run_queue::fair_balance::MIN_INTERVAL_NS;
}
kcpu_slot::cpu_slot_cell! {
    static BALANCE_CPU_CURSOR: usize = 0;
}
kcpu_slot::cpu_slot_cell! {
    static IDLE_CPU_CURSOR: usize = 0;
}

/// Remote ranking hints only; no task state or ownership is published here.
///
/// Independent Relaxed samples may describe different RQ updates. Selection
/// tolerates this: the source refreshes and rechecks under its own lock, and
/// destination publish validates affinity again. A mixed sample can only
/// cause a missed/extra balance attempt, never authorize an unsafe migration.
pub(super) struct BalanceState {
    load_avg: AtomicU64,
    nr_queued: AtomicUsize,
    /// Release/Acquire carries the balance request to the reschedule IPI.
    /// There is no callback allocation or associated task payload.
    kick_pending: AtomicBool,
    /// Advisory rate limit, checked and set under the source scheduler lock.
    last_migration_ns: AtomicU64,
}

impl BalanceState {
    pub(super) const fn new() -> Self {
        Self {
            load_avg: AtomicU64::new(0),
            nr_queued: AtomicUsize::new(0),
            kick_pending: AtomicBool::new(false),
            last_migration_ns: AtomicU64::new(0),
        }
    }

    pub(super) fn publish(&self, stats: FairBalanceStats) {
        self.load_avg.store(stats.pelt.load_avg, Ordering::Relaxed);
        self.nr_queued.store(stats.nr_queued, Ordering::Relaxed);
    }

    fn sample(&self) -> LoadHint {
        LoadHint {
            load_avg: self.load_avg.load(Ordering::Relaxed),
            nr_queued: self.nr_queued.load(Ordering::Relaxed),
        }
    }

    fn is_cooling_down(&self, now_ns: u64) -> bool {
        let last_ns = self.last_migration_ns.load(Ordering::Relaxed);
        last_ns != 0 && now_ns.saturating_sub(last_ns) < MIGRATION_COOLDOWN_NS
    }
}

#[derive(Clone, Copy)]
struct LoadHint {
    load_avg: u64,
    nr_queued: usize,
}

/// Register once during primary RQ bring-up, before timer/IPI delivery.
pub(super) fn init() {
    assert!(
        kirq::softirq::open_softirq(SoftirqVec::Sched, run_balance_softirq),
        "scheduler softirq already registered"
    );
}

/// Request periodic work after local accounting, with IRQs/preemption disabled.
/// No RQ lock may be held. Missed periods coalesce instead of being replayed.
pub(crate) fn on_periodic_tick(now_ns: u64) {
    let next_ns = NEXT_BALANCE_NS.load_current();
    if next_ns == 0 {
        let phase_ns = current_run_queue_ref().cpu_id.as_usize() as u64 * MIN_INTERVAL_NS
            / kcpu_id_map::nr_cpus().max(1) as u64;
        NEXT_BALANCE_NS.store_current(now_ns.saturating_add(MIN_INTERVAL_NS + phase_ns));
    } else if now_ns >= next_ns {
        NEXT_BALANCE_NS.store_current(now_ns.saturating_add(BALANCE_INTERVAL_NS.load_current()));
        raise_softirq_irqoff(SoftirqVec::Sched);
    }
}

/// Consume a balance kick inside the reschedule IPI, without taking an RQ lock.
pub(crate) fn on_reschedule_ipi() {
    if current_run_queue_ref()
        .balance
        .kick_pending
        .swap(false, Ordering::Acquire)
    {
        raise_softirq_irqoff(SoftirqVec::Sched);
    }
}

fn run_balance_softirq() {
    let now_ns = khal::time::monotonic_time_nanos();
    let destination = current_run_queue_ref();
    #[cfg(feature = "sched_stat")]
    super::stats::sched_stat_inc(
        &super::stats::sched_stat_cpu(destination.cpu_id).fair_balance_runs,
    );

    let local = {
        // Local accounting must retain the ordinary user/cgroup side effects;
        // remote queue refresh alone only advances scheduler history.
        let mut rq = super::current_run_queue::<NoPreemptIrqSave>();
        rq.account_current_runtime(now_ns);
        destination.with_sched(|s| s.fair_balance_stats())
    };
    let did_pull = (local.is_idle || !destination.balance.is_cooling_down(now_ns))
        && destination.pull_fair_task(local, now_ns);

    // Idle CPUs stop their periodic tick. An overloaded CPU supplies the
    // missing trigger; the target performs its own pull in softirq context.
    if local.nr_queued != 0 && !did_pull {
        kick_idle_cpu(destination.cpu_id, now_ns);
    }
    {
        let _guard = NoPreemptIrqSave::new();
        let interval_ns = if did_pull {
            MIN_INTERVAL_NS
        } else {
            (BALANCE_INTERVAL_NS.load_current() * 2).min(MAX_INTERVAL_NS)
        };
        BALANCE_INTERVAL_NS.store_current(interval_ns);
        NEXT_BALANCE_NS.store_current(now_ns.saturating_add(interval_ns));
    }
}

impl RunQueue {
    fn pull_fair_task(&self, local: FairBalanceStats, now_ns: u64) -> bool {
        let mut tried = KCpuMask::new();
        for _ in 0..MAX_SOURCE_ATTEMPTS {
            let Some(source) = find_source(self.cpu_id, local, &tried) else {
                return false;
            };
            tried.set(source.cpu_id.as_usize(), true);
            let stolen = {
                let _guard = NoPreemptIrqSave::new();
                source.with_sched_rank(|s| {
                    if source.balance.is_cooling_down(now_ns) {
                        return None;
                    }
                    let _elapsed = Self::advance_account_epoch(&source.last_accounted_ns, now_ns);
                    let stolen = s.steal_fair(
                        MAX_TASKS_PER_SCAN,
                        &mut |task, pelt, fresh| {
                            can_idle_pull_task(task, self.cpu_id.as_usize())
                                && migration_improves_balance(fresh, local, pelt)
                        },
                        now_ns,
                    );
                    if stolen.is_some() {
                        source
                            .balance
                            .last_migration_ns
                            .store(now_ns, Ordering::Relaxed);
                    }
                    stolen
                })
            };
            let Some(stolen) = stolen else {
                continue;
            };
            let _guard = NoPreemptIrqSave::new();
            match self.enqueue_task(stolen) {
                Ok(decision) => {
                    self.balance
                        .last_migration_ns
                        .store(now_ns, Ordering::Relaxed);
                    self.kick(decision);
                    #[cfg(feature = "sched_stat")]
                    super::stats::sched_stat_inc(
                        &super::stats::sched_stat_cpu(self.cpu_id).fair_balance_pull,
                    );
                    return true;
                }
                Err(rejected) => migrate_entry(rejected, ksched::EnqueueKind::MigrateIn),
            }
            return false;
        }
        false
    }
}

fn find_source(
    destination: LogicalCpuId,
    local: FairBalanceStats,
    tried: &KCpuMask,
) -> Option<&'static RunQueue> {
    let cpu = find_source_cpu(destination.as_usize(), local, tried, scan_cpus(), |cpu| {
        RUN_QUEUES.try_get(cpu).map(|rq| rq.balance.sample())
    })?;
    // Always retrying the heaviest pinned source would hide lighter sources
    // that can actually donate. Continue after each attempted CPU, even when
    // the entire machine fits within one scan batch.
    let _guard = NoPreemptIrqSave::new();
    BALANCE_CPU_CURSOR.store_current((cpu + 1) % kcpu_id_map::nr_cpus().max(1));
    RUN_QUEUES.try_get(cpu)
}

fn find_source_cpu(
    destination: usize,
    local: FairBalanceStats,
    tried: &KCpuMask,
    mut cpus: impl Iterator<Item = usize>,
    mut load_of: impl FnMut(usize) -> Option<LoadHint>,
) -> Option<usize> {
    cpus.find(|&cpu| {
        cpu != destination
            && !tried.get(cpu)
            && load_of(cpu).is_some_and(|hint| is_busier(hint, local))
    })
}

fn is_busier(source: LoadHint, destination: FairBalanceStats) -> bool {
    if source.nr_queued == 0 {
        return false;
    }
    destination.is_idle
        || source.load_avg.saturating_sub(destination.pelt.load_avg) > (source.load_avg / 16).max(1)
}

fn migration_improves_balance(
    source: FairBalanceStats,
    destination: FairBalanceStats,
    task: PeltAvg,
) -> bool {
    if source.nr_queued == 0 {
        return false;
    }
    if destination.is_idle {
        return true;
    }
    let source_load = i128::from(source.pelt.load_avg);
    let destination_load = i128::from(destination.pelt.load_avg);
    let difference = source_load - destination_load;
    let after = (difference - 2 * i128::from(task.load_avg.max(1))).abs();
    // A relative margin also works when every task has a very small nice
    // weight. An absolute nice-0 threshold would strand nice-19 workloads.
    let margin = ((source_load + destination_load) / 32).max(1);
    difference > 0 && difference - after > margin
}

fn kick_idle_cpu(source: LogicalCpuId, now_ns: u64) {
    for cpu in scan_idle_cpus() {
        if cpu == source.as_usize() {
            continue;
        }
        let Some(target) = RUN_QUEUES.try_get(cpu) else {
            continue;
        };
        if target.nr_running.load(Ordering::Relaxed) != 0 || target.balance.is_cooling_down(now_ns)
        {
            continue;
        }
        if target
            .balance
            .kick_pending
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            continue;
        }
        {
            let _guard = NoPreemptIrqSave::new();
            IDLE_CPU_CURSOR.store_current((cpu + 1) % kcpu_id_map::nr_cpus().max(1));
        }
        if super::rq::request_remote_resched(target.cpu_id).is_err() {
            target.balance.kick_pending.store(false, Ordering::Relaxed);
        } else {
            #[cfg(feature = "sched_stat")]
            super::stats::sched_stat_inc(&super::stats::sched_stat_cpu(source).fair_balance_kick);
        }
        return;
    }
}

/// Rotate bounded CPU batches so a large machine eventually visits every RQ.
fn scan_cpus() -> impl Iterator<Item = usize> {
    let _guard = NoPreemptIrqSave::new();
    let nr_cpus = kcpu_id_map::nr_cpus().max(1);
    let start = BALANCE_CPU_CURSOR.load_current() % nr_cpus;
    let count = nr_cpus.min(MAX_CPUS_PER_SCAN);
    BALANCE_CPU_CURSOR.store_current((start + count) % nr_cpus);
    (0..count).map(move |offset| (start + offset) % nr_cpus)
}

fn scan_idle_cpus() -> impl Iterator<Item = usize> {
    let _guard = NoPreemptIrqSave::new();
    let nr_cpus = kcpu_id_map::nr_cpus().max(1);
    let start = IDLE_CPU_CURSOR.load_current() % nr_cpus;
    let count = nr_cpus.min(MAX_CPUS_PER_SCAN);
    IDLE_CPU_CURSOR.store_current((start + count) % nr_cpus);
    (0..count).map(move |offset| (start + offset) % nr_cpus)
}

unittest::path_mod_test!(tests, "../../ut-tests/run_queue/fair_balance.rs");
