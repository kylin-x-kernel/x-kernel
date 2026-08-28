// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Feature-gated workqueue stress commands.
//!
//! This module intentionally exercises only the public `kwork` product API.
//! Worker-pool scheduling state and workqueue core accounting remain behind
//! their normal runtime boundaries.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    str,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use ktime_types::TimeSpan;

use crate::{
    BudgetedPollProgress, BudgetedPoller, CancelWorkResult, DelayedScheduledWork,
    QueueDelayedWorkResult, QueueWorkResult, ScheduleAttrs, ScheduledWork, WorkQueue,
    WorkQueueAttrs, WorkQueueHandle, WorkqueueError,
    builtinpool::{self, SystemPoolKind},
    runtime, system_bh_highpri_wq, system_percpu_wq, system_wq,
};

const DEFAULT_ROUNDS: usize = 32;
const DEFAULT_WORKS: usize = 64;
const MAX_ROUNDS: usize = 4096;
const MAX_WORKS: usize = 512;
const DEFAULT_SOAK_SECONDS: usize = 60;
const MAX_SOAK_SECONDS: usize = 24 * 60 * 60;
const DEFAULT_BENCH_SECONDS: usize = 5;
const MAX_BENCH_SECONDS: usize = 60 * 60;
const SOAK_DYNAMIC_QUEUE_INTERVAL: usize = 16;
const WAIT_TIMEOUT: TimeSpan = TimeSpan::from_secs(30);

static STATIC_STRESS_WQ: WorkQueue = WorkQueue::new("kwork_stress_static_wq");
static CURRENT_CASE: AtomicUsize = AtomicUsize::new(0);
static CURRENT_ROUND: AtomicUsize = AtomicUsize::new(0);
static CURRENT_PHASE: AtomicUsize = AtomicUsize::new(0);

const CASE_NONE: usize = 0;
const CASE_QUEUE_FLUSH: usize = 1;
const CASE_STATIC_QUEUE: usize = 2;
const CASE_BALANCED_FANOUT: usize = 3;
const CASE_PERCPU_FANOUT: usize = 4;
const CASE_YIELD_CPU: usize = 5;
const CASE_BH_DRAIN: usize = 6;
const CASE_BH_HIGHPRI: usize = 7;
const CASE_BUDGETED_POLLER: usize = 8;
const CASE_CANCEL_RACE: usize = 9;
const CASE_DISABLE_RACE: usize = 10;
const CASE_DESTROY_RACE: usize = 11;
const CASE_DELAYED_CANCEL: usize = 12;
const CASE_CANCEL_NONBLOCKING: usize = 13;
const CASE_WAIT_DEADLOCK: usize = 14;
const CASE_SLEEP_BLOCK: usize = 15;

const PHASE_NONE: usize = 0;
const PHASE_CANCEL_SPAWN: usize = 1;
const PHASE_CANCEL_WAIT_TASKS: usize = 2;
const PHASE_CANCEL_FINAL_FLUSH: usize = 3;
const PHASE_BUDGETED_NOTIFY: usize = 4;
const PHASE_BUDGETED_WAIT: usize = 5;
const PHASE_BUDGETED_DESTROY: usize = 6;

/// Result of one stress command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StressSummary {
    pub case: &'static str,
    pub rounds: usize,
    pub queued: usize,
    pub completed: usize,
    pub cancel: usize,
    pub cancel_sync: usize,
    pub would_deadlock: usize,
    pub disabled: usize,
    pub failures: usize,
    pub active_cpus: usize,
}

impl StressSummary {
    const fn new(case: &'static str) -> Self {
        Self {
            case,
            rounds: 0,
            queued: 0,
            completed: 0,
            cancel: 0,
            cancel_sync: 0,
            would_deadlock: 0,
            disabled: 0,
            failures: 0,
            active_cpus: 0,
        }
    }

    fn merge(&mut self, other: &Self) {
        self.queued = self.queued.saturating_add(other.queued);
        self.completed = self.completed.saturating_add(other.completed);
        self.cancel = self.cancel.saturating_add(other.cancel);
        self.cancel_sync = self.cancel_sync.saturating_add(other.cancel_sync);
        self.would_deadlock = self.would_deadlock.saturating_add(other.would_deadlock);
        self.disabled = self.disabled.saturating_add(other.disabled);
        self.failures = self.failures.saturating_add(other.failures);
        self.active_cpus = self.active_cpus.max(other.active_cpus);
    }

    fn text(&self) -> String {
        let mut out = format!(
            "case={} rounds={} queued={} completed={}",
            self.case, self.rounds, self.queued, self.completed
        );
        if self.cancel_sync != 0 {
            out.push_str(&format!(" cancel_sync={}", self.cancel_sync));
        }
        if self.cancel != 0 {
            out.push_str(&format!(" cancel={}", self.cancel));
        }
        if self.would_deadlock != 0 {
            out.push_str(&format!(" would_deadlock={}", self.would_deadlock));
        }
        if self.disabled != 0 {
            out.push_str(&format!(" disabled={}", self.disabled));
        }
        if self.failures != 0 {
            out.push_str(&format!(" failures={}", self.failures));
        }
        if self.active_cpus != 0 {
            out.push_str(&format!(" active_cpus={}", self.active_cpus));
        }
        out.push('\n');
        out
    }
}

struct BenchSummary {
    case: &'static str,
    seconds: usize,
    batches: usize,
    ops: usize,
    elapsed_ns: u64,
    active_cpus: usize,
}

impl BenchSummary {
    fn text(&self) -> String {
        let ns_per_op = if self.ops == 0 {
            0
        } else {
            self.elapsed_ns / self.ops as u64
        };
        let ops_per_sec = (self.ops as u64)
            .saturating_mul(1_000_000_000)
            .checked_div(self.elapsed_ns)
            .unwrap_or(0);
        format!(
            "bench={} seconds={} batches={} ops={} elapsed_ns={} ns_per_op={} ops_per_sec={} \
             active_cpus={}\n",
            self.case,
            self.seconds,
            self.batches,
            self.ops,
            self.elapsed_ns,
            ns_per_op,
            ops_per_sec,
            self.active_cpus,
        )
    }
}

/// Error returned by a stress command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StressError {
    InvalidCommand,
    InvalidArgument,
    PoolNotReady,
    QueueFailed(QueueWorkResult),
    FlushFailed(WorkqueueError),
    Timeout,
    Unbalanced,
    Incomplete,
    CaseFailed {
        case: &'static str,
        reason: StressFailureReason,
    },
}

/// Compact reason stored when a combined stress command fails inside a subcase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StressFailureReason {
    InvalidCommand,
    InvalidArgument,
    PoolNotReady,
    QueueFailed,
    QueueAlreadyQueued,
    QueueFull,
    QueueDisabled,
    QueueInvalidCpu,
    QueueWorkerUnavailable,
    FlushFailed,
    Timeout,
    Unbalanced,
    Incomplete,
}

/// Returns a short description suitable for a procfs read.
pub fn stress_status_text() -> String {
    let mut out = "kwork stress commands: queue-flush [rounds] [works], static-queue [rounds] \
                   [works], balanced-fanout [rounds] [works], percpu-fanout [rounds] [works], \
                   yield-cpu [rounds], bh-drain [rounds], bh-highpri [rounds], budgeted-poller \
                   [rounds] [works], cancel-race [rounds] [works], disable-race [rounds] [works], \
                   destroy-race [rounds] [works], delayed-cancel [rounds] [works], \
                   cancel-nonblocking [rounds] [works], wait-deadlock [rounds], sleep-block \
                   [rounds], all [rounds] [works], smoke, soak [seconds] [works], bench [seconds] \
                   [works], dump\n"
        .to_string();
    let case = CURRENT_CASE.load(Ordering::Acquire);
    let phase = CURRENT_PHASE.load(Ordering::Acquire);
    if case != CASE_NONE || phase != PHASE_NONE {
        out.push_str(&format!(
            "running case={} round={} phase={}\n",
            case_name(case),
            CURRENT_ROUND.load(Ordering::Acquire),
            phase_name(phase)
        ));
        append_compact_state(&mut out);
    }
    out
}

fn append_compact_state(out: &mut String) {
    for cpu in 0..kbuild_config::NR_CPUS {
        let cpu_id = LogicalCpuId::new(cpu);
        let Some(pool) = builtinpool::system_pool_for_kind_cpu(SystemPoolKind::Normal, cpu_id)
        else {
            out.push_str(&format!("normal_pool cpu={cpu} unavailable\n"));
            continue;
        };
        let snapshot = pool.pool().lock().snapshot();
        out.push_str(&format!(
            "normal_pool cpu={cpu} installed={} idle={} preparing={} claiming={} running={} \
             runnable={} deferred={} workers={:?}\n",
            snapshot.installed_workers,
            snapshot.nr_idle,
            snapshot.nr_preparing,
            snapshot.nr_claiming,
            snapshot.nr_running_state,
            snapshot.runnable,
            snapshot.deferred,
            snapshot.worker_states,
        ));
    }
    for queue_ref in crate::work::registered_workqueues() {
        let queue = queue_ref.queue();
        for cpu in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu);
            let Some(binding) = queue.core_binding(cpu_id) else {
                continue;
            };
            let snapshot = binding.snapshot();
            if snapshot.active == 0 && snapshot.pending == 0 {
                continue;
            }
            out.push_str(&format!(
                "wq name={} cpu={cpu} active={} pending={} in_flight={:?}\n",
                queue.name(),
                snapshot.active,
                snapshot.pending,
                snapshot.in_flight,
            ));
        }
    }
}

/// Runs one stress command from a debug/proc control file payload.
pub fn run_stress_command(data: &[u8]) -> Result<String, StressError> {
    let input = str::from_utf8(data).map_err(|_| StressError::InvalidArgument)?;
    let mut tokens = input.split_ascii_whitespace();
    let Some(command) = tokens.next() else {
        return Ok(stress_status_text());
    };
    let args = tokens.collect::<Vec<_>>();
    if command == "dump" {
        return Ok(dump_state());
    }
    if command == "bench" {
        return run_bench(args.as_slice());
    }
    let _guard = StressCaseGuard::enter(case_id(command));
    let summaries = match command {
        "queue-flush" => vec![run_queue_flush(args.as_slice())?],
        "static-queue" => vec![run_static_queue(args.as_slice())?],
        "balanced-fanout" => vec![run_balanced_fanout(args.as_slice())?],
        "percpu-fanout" => vec![run_percpu_fanout(args.as_slice())?],
        "yield-cpu" => vec![run_yield_cpu(args.as_slice())?],
        "bh-drain" => vec![run_bh_drain(args.as_slice())?],
        "bh-highpri" => vec![run_bh_highpri(args.as_slice())?],
        "budgeted-poller" => vec![run_budgeted_poller(args.as_slice())?],
        "cancel-race" => vec![run_cancel_race(args.as_slice())?],
        "disable-race" => vec![run_disable_race(args.as_slice())?],
        "destroy-race" => vec![run_destroy_race(args.as_slice())?],
        "delayed-cancel" => vec![run_delayed_cancel(args.as_slice())?],
        "cancel-nonblocking" => vec![run_cancel_nonblocking(args.as_slice())?],
        "wait-deadlock" => vec![run_wait_deadlock(args.as_slice())?],
        "sleep-block" => vec![run_sleep_block(args.as_slice())?],
        "all" => run_all(args.as_slice())?,
        "smoke" => run_smoke()?,
        "soak" => vec![run_soak(args.as_slice())?],
        "help" | "status" => return Ok(stress_status_text()),
        _ => return Err(StressError::InvalidCommand),
    };

    let mut out = String::new();
    for summary in summaries {
        out.push_str(&summary.text());
    }
    Ok(out)
}

fn dump_state() -> String {
    let mut out = String::from("kwork stress state\n");
    for kind in [SystemPoolKind::Normal, SystemPoolKind::Bh] {
        for cpu in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu);
            let Some(pool) = builtinpool::system_pool_for_kind_cpu(kind, cpu_id) else {
                out.push_str(&format!("pool kind={kind:?} cpu={cpu} unavailable\n"));
                continue;
            };
            let snapshot = pool.pool().lock().snapshot();
            out.push_str(&format!(
                "pool kind={kind:?} cpu={cpu} installed={} creating={} idle={} preparing={} \
                 claiming={} running={} sleeping={} retire={} exiting={} concurrency={} \
                 runnable={} deferred={} workers={:?}\n",
                snapshot.installed_workers,
                snapshot.nr_creating,
                snapshot.nr_idle,
                snapshot.nr_preparing,
                snapshot.nr_claiming,
                snapshot.nr_running_state,
                snapshot.nr_sleeping,
                snapshot.nr_retire_requested,
                snapshot.nr_exiting,
                snapshot.nr_concurrency,
                snapshot.runnable,
                snapshot.deferred,
                snapshot.worker_states,
            ));
        }
    }
    for queue_ref in crate::work::registered_workqueues() {
        let queue = queue_ref.queue();
        for cpu in 0..kbuild_config::NR_CPUS {
            let cpu_id = LogicalCpuId::new(cpu);
            let Some(binding) = queue.core_binding(cpu_id) else {
                continue;
            };
            let snapshot = binding.snapshot();
            out.push_str(&format!(
                "wq name={} cpu={cpu} active={} color={:?} in_flight={:?} pending={}\n",
                queue.name(),
                snapshot.active,
                snapshot.current_color,
                snapshot.in_flight,
                snapshot.pending,
            ));
        }
    }
    out
}

fn run_bench(args: &[&str]) -> Result<String, StressError> {
    let seconds = parse_arg(args, 0, DEFAULT_BENCH_SECONDS, 1, MAX_BENCH_SECONDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let mut out = String::new();
    for summary in [
        bench_system_balanced(seconds, works)?,
        bench_system_percpu(seconds, works)?,
        bench_static_queue(seconds, works)?,
        bench_dynamic_serial(seconds, works)?,
        bench_bh_default(seconds, works)?,
        bench_budgeted_poller(seconds, works)?,
    ] {
        out.push_str(&summary.text());
    }
    Ok(out)
}

fn run_bench_case(
    case: &'static str,
    seconds: usize,
    mut run_batch: impl FnMut() -> Result<(usize, usize), StressError>,
) -> Result<BenchSummary, StressError> {
    let start_ns = ktask::monotonic_time().as_nanos_u64_saturating();
    let deadline = ktask::monotonic_time() + TimeSpan::from_secs(seconds as u64);
    let mut batches = 0usize;
    let mut ops = 0usize;
    let mut active_cpus = 0usize;

    loop {
        let (batch_ops, batch_active_cpus) = run_batch()?;
        batches = batches.saturating_add(1);
        ops = ops.saturating_add(batch_ops);
        active_cpus = active_cpus.max(batch_active_cpus);
        if ktask::monotonic_time() >= deadline {
            break;
        }
        ktask::yield_now();
    }

    let end_ns = ktask::monotonic_time().as_nanos_u64_saturating();
    Ok(BenchSummary {
        case,
        seconds,
        batches,
        ops,
        elapsed_ns: end_ns.saturating_sub(start_ns),
        active_cpus,
    })
}

fn bench_system_balanced(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    run_bench_case("system-balanced", seconds, || {
        let completed = Arc::new(AtomicUsize::new(0));
        let per_cpu = per_cpu_counters();
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(system_wq().queue_work(work))?;
        }
        for work in &batch {
            work.flush().map_err(StressError::FlushFailed)?;
        }
        Ok((
            completed.load(Ordering::Acquire),
            active_cpu_count(&per_cpu),
        ))
    })
}

fn bench_system_percpu(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let mut cursor = 0usize;
    run_bench_case("system-percpu", seconds, || {
        let cpu_id = cpus[cursor % cpus.len()];
        cursor = cursor.wrapping_add(1);
        let completed = Arc::new(AtomicUsize::new(0));
        let per_cpu = per_cpu_counters();
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(system_percpu_wq().queue_work_on(cpu_id, work))?;
        }
        for work in &batch {
            work.flush().map_err(StressError::FlushFailed)?;
        }
        Ok((
            completed.load(Ordering::Acquire),
            active_cpu_count(&per_cpu),
        ))
    })
}

fn bench_static_queue(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let mut cursor = 0usize;
    run_bench_case("static-queue", seconds, || {
        let cpu_id = cpus[cursor % cpus.len()];
        cursor = cursor.wrapping_add(1);
        let completed = Arc::new(AtomicUsize::new(0));
        let per_cpu = per_cpu_counters();
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(STATIC_STRESS_WQ.queue_work_on(cpu_id, work))?;
        }
        STATIC_STRESS_WQ.flush().map_err(StressError::FlushFailed)?;
        Ok((
            completed.load(Ordering::Acquire),
            active_cpu_count(&per_cpu),
        ))
    })
}

fn bench_dynamic_serial(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let queue = WorkQueueHandle::alloc(
        "kwork_bench_dynamic_serial",
        WorkQueueAttrs::new().with_max_active(1),
    )
    .map_err(|_| StressError::PoolNotReady)?;
    let mut cursor = 0usize;
    let result = run_bench_case("dynamic-serial", seconds, || {
        let cpu_id = cpus[cursor % cpus.len()];
        cursor = cursor.wrapping_add(1);
        let completed = Arc::new(AtomicUsize::new(0));
        let per_cpu = per_cpu_counters();
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(queue.queue_work_on(cpu_id, work))?;
        }
        queue.flush().map_err(StressError::FlushFailed)?;
        Ok((
            completed.load(Ordering::Acquire),
            active_cpu_count(&per_cpu),
        ))
    });
    queue.destroy().map_err(StressError::FlushFailed)?;
    result
}

fn bench_bh_default(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    let cpu_id = stress_cpu(SystemPoolKind::Bh)?;
    run_bench_case("bh-default", seconds, || {
        let completed = Arc::new(AtomicUsize::new(0));
        let per_cpu = per_cpu_counters();
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(work.schedule_with(ScheduleAttrs::bottom_half().on_cpu(cpu_id)))?;
        }
        wait_until(|| completed.load(Ordering::Acquire) >= works)?;
        Ok((
            completed.load(Ordering::Acquire),
            active_cpu_count(&per_cpu),
        ))
    })
}

fn bench_budgeted_poller(seconds: usize, works: usize) -> Result<BenchSummary, StressError> {
    let works = works.to_string();
    run_bench_case("budgeted-poller", seconds, || {
        let summary = run_budgeted_poller(&["8", works.as_str()])?;
        Ok((summary.completed, summary.active_cpus))
    })
}

fn run_all(args: &[&str]) -> Result<Vec<StressSummary>, StressError> {
    Ok(vec![
        run_queue_flush(args)?,
        run_static_queue(args)?,
        run_balanced_fanout(args)?,
        run_percpu_fanout(args)?,
        run_yield_cpu(args)?,
        run_bh_drain(args)?,
        run_bh_highpri(args)?,
        run_budgeted_poller(args)?,
        run_cancel_race(args)?,
        run_disable_race(args)?,
        run_destroy_race(args)?,
        run_delayed_cancel(args)?,
        run_cancel_nonblocking(args)?,
        run_wait_deadlock(args)?,
        run_sleep_block(args)?,
    ])
}

fn run_smoke() -> Result<Vec<StressSummary>, StressError> {
    Ok(vec![
        run_queue_flush(&["4", "8"])?,
        run_static_queue(&["4", "8"])?,
        run_balanced_fanout(&["4", "16"])?,
        run_percpu_fanout(&["4", "8"])?,
        run_yield_cpu(&["4"])?,
        run_bh_drain(&["4"])?,
        run_bh_highpri(&["4"])?,
        run_budgeted_poller(&["4", "8"])?,
        run_cancel_race(&["2", "8"])?,
        run_disable_race(&["2", "8"])?,
        run_destroy_race(&["2", "8"])?,
        run_delayed_cancel(&["2", "8"])?,
        run_cancel_nonblocking(&["2", "8"])?,
        run_wait_deadlock(&["2"])?,
        run_sleep_block(&["2"])?,
    ])
}

fn run_soak(args: &[&str]) -> Result<StressSummary, StressError> {
    let seconds = parse_arg(args, 0, DEFAULT_SOAK_SECONDS, 1, MAX_SOAK_SECONDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let deadline = ktask::monotonic_time() + TimeSpan::from_secs(seconds as u64);
    let works = works.to_string();
    let case_args = ["4", works.as_str()];
    let mut summary = StressSummary::new("soak");

    loop {
        CURRENT_ROUND.store(summary.rounds, Ordering::Release);
        for case_summary in run_soak_cycle(&case_args, summary.rounds)? {
            summary.merge(&case_summary);
        }
        summary.rounds += 1;
        ktask::yield_now();
        if ktask::monotonic_time() >= deadline {
            return Ok(summary);
        }
    }
}

fn run_soak_cycle(args: &[&str], iteration: usize) -> Result<Vec<StressSummary>, StressError> {
    let mut summaries = vec![run_named_case("balanced-fanout", || {
        run_balanced_fanout(args)
    })?];
    summaries.push(run_named_case("percpu-fanout", || run_percpu_fanout(args))?);
    summaries.push(run_named_case("yield-cpu", || run_yield_cpu(args))?);
    summaries.push(run_named_case("bh-drain", || run_bh_drain(args))?);
    summaries.push(run_named_case("bh-highpri", || run_bh_highpri(args))?);
    summaries.push(run_named_case("budgeted-poller", || {
        run_budgeted_poller(args)
    })?);
    summaries.push(run_named_case("cancel-race", || run_cancel_race(args))?);
    summaries.push(run_named_case("disable-race", || run_disable_race(args))?);
    summaries.push(run_named_case("delayed-cancel", || {
        run_delayed_cancel(args)
    })?);
    summaries.push(run_named_case("cancel-nonblocking", || {
        run_cancel_nonblocking(args)
    })?);
    summaries.push(run_named_case("wait-deadlock", || {
        run_wait_deadlock(&["4"])
    })?);
    summaries.push(run_named_case("sleep-block", || run_sleep_block(&["4"]))?);
    if iteration.is_multiple_of(SOAK_DYNAMIC_QUEUE_INTERVAL) {
        summaries.push(run_named_case("queue-flush", || run_queue_flush(args))?);
        summaries.push(run_named_case("static-queue", || run_static_queue(args))?);
        summaries.push(run_named_case("destroy-race", || run_destroy_race(args))?);
    }
    Ok(summaries)
}

fn run_named_case(
    case: &'static str,
    f: impl FnOnce() -> Result<StressSummary, StressError>,
) -> Result<StressSummary, StressError> {
    let _guard = StressCaseGuard::enter(case_id(case));
    match f() {
        Ok(summary) => Ok(summary),
        Err(error) => Err(StressError::CaseFailed {
            case,
            reason: error.reason(),
        }),
    }
}

struct StressCaseGuard {
    previous_case: usize,
    previous_phase: usize,
}

impl StressCaseGuard {
    fn enter(case: usize) -> Self {
        let previous_case = CURRENT_CASE.swap(case, Ordering::AcqRel);
        let previous_phase = CURRENT_PHASE.swap(PHASE_NONE, Ordering::AcqRel);
        Self {
            previous_case,
            previous_phase,
        }
    }
}

impl Drop for StressCaseGuard {
    fn drop(&mut self) {
        CURRENT_CASE.store(self.previous_case, Ordering::Release);
        CURRENT_PHASE.store(self.previous_phase, Ordering::Release);
    }
}

fn case_id(case: &str) -> usize {
    match case {
        "queue-flush" => CASE_QUEUE_FLUSH,
        "static-queue" => CASE_STATIC_QUEUE,
        "balanced-fanout" => CASE_BALANCED_FANOUT,
        "percpu-fanout" => CASE_PERCPU_FANOUT,
        "yield-cpu" => CASE_YIELD_CPU,
        "bh-drain" => CASE_BH_DRAIN,
        "bh-highpri" => CASE_BH_HIGHPRI,
        "budgeted-poller" => CASE_BUDGETED_POLLER,
        "cancel-race" => CASE_CANCEL_RACE,
        "disable-race" => CASE_DISABLE_RACE,
        "destroy-race" => CASE_DESTROY_RACE,
        "delayed-cancel" => CASE_DELAYED_CANCEL,
        "cancel-nonblocking" => CASE_CANCEL_NONBLOCKING,
        "wait-deadlock" => CASE_WAIT_DEADLOCK,
        "sleep-block" => CASE_SLEEP_BLOCK,
        _ => CASE_NONE,
    }
}

fn case_name(case: usize) -> &'static str {
    match case {
        CASE_QUEUE_FLUSH => "queue-flush",
        CASE_STATIC_QUEUE => "static-queue",
        CASE_BALANCED_FANOUT => "balanced-fanout",
        CASE_PERCPU_FANOUT => "percpu-fanout",
        CASE_YIELD_CPU => "yield-cpu",
        CASE_BH_DRAIN => "bh-drain",
        CASE_BH_HIGHPRI => "bh-highpri",
        CASE_BUDGETED_POLLER => "budgeted-poller",
        CASE_CANCEL_RACE => "cancel-race",
        CASE_DISABLE_RACE => "disable-race",
        CASE_DESTROY_RACE => "destroy-race",
        CASE_DELAYED_CANCEL => "delayed-cancel",
        CASE_CANCEL_NONBLOCKING => "cancel-nonblocking",
        CASE_WAIT_DEADLOCK => "wait-deadlock",
        CASE_SLEEP_BLOCK => "sleep-block",
        _ => "none",
    }
}

fn phase_name(phase: usize) -> &'static str {
    match phase {
        PHASE_CANCEL_SPAWN => "cancel-spawn",
        PHASE_CANCEL_WAIT_TASKS => "cancel-wait-tasks",
        PHASE_CANCEL_FINAL_FLUSH => "cancel-final-flush",
        PHASE_BUDGETED_NOTIFY => "budgeted-notify",
        PHASE_BUDGETED_WAIT => "budgeted-wait",
        PHASE_BUDGETED_DESTROY => "budgeted-destroy",
        _ => "none",
    }
}

impl StressError {
    fn reason(self) -> StressFailureReason {
        match self {
            StressError::InvalidCommand => StressFailureReason::InvalidCommand,
            StressError::InvalidArgument => StressFailureReason::InvalidArgument,
            StressError::PoolNotReady => StressFailureReason::PoolNotReady,
            StressError::QueueFailed(result) => queue_failure_reason(result),
            StressError::FlushFailed(_) => StressFailureReason::FlushFailed,
            StressError::Timeout => StressFailureReason::Timeout,
            StressError::Unbalanced => StressFailureReason::Unbalanced,
            StressError::Incomplete => StressFailureReason::Incomplete,
            StressError::CaseFailed { reason, .. } => reason,
        }
    }
}

fn queue_failure_reason(result: QueueWorkResult) -> StressFailureReason {
    match result {
        QueueWorkResult::Queued => StressFailureReason::QueueFailed,
        QueueWorkResult::AlreadyQueued => StressFailureReason::QueueAlreadyQueued,
        QueueWorkResult::QueueFull => StressFailureReason::QueueFull,
        QueueWorkResult::Disabled => StressFailureReason::QueueDisabled,
        QueueWorkResult::InvalidCpu => StressFailureReason::QueueInvalidCpu,
        QueueWorkResult::WorkerUnavailable => StressFailureReason::QueueWorkerUnavailable,
    }
}

fn run_queue_flush(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let queue = WorkQueueHandle::alloc(
        "kwork_stress_queue_flush",
        WorkQueueAttrs::new().with_max_active(1),
    )
    .map_err(|_| StressError::PoolNotReady)?;
    let mut queued = 0;

    for round in 0..rounds {
        let cpu_id = cpus[round % cpus.len()];
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            match queue.queue_work_on(cpu_id, work) {
                QueueWorkResult::Queued => queued += 1,
                other => return Err(StressError::QueueFailed(other)),
            }
        }
        queue.flush().map_err(StressError::FlushFailed)?;
    }
    queue.destroy().map_err(StressError::FlushFailed)?;

    Ok(StressSummary {
        case: "queue-flush",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_static_queue(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut queued = 0;

    for round in 0..rounds {
        let cpu_id = cpus[round % cpus.len()];
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(STATIC_STRESS_WQ.queue_work_on(cpu_id, work))?;
            queued += 1;
        }
        STATIC_STRESS_WQ.flush().map_err(StressError::FlushFailed)?;
    }

    Ok(StressSummary {
        case: "static-queue",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_balanced_fanout(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let ready = ready_cpus(SystemPoolKind::Normal)?.len();
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut queued = 0;

    for _ in 0..rounds {
        let mut batch = Vec::new();
        for _ in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            batch.push(ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            }));
        }
        for work in &batch {
            expect_queued(system_wq().queue_work(work))?;
            queued += 1;
        }
        for work in &batch {
            work.flush().map_err(StressError::FlushFailed)?;
        }
    }

    let active_cpus = active_cpu_count(&per_cpu);
    if ready > 1 && queued >= ready && active_cpus < 2 {
        return Err(StressError::Unbalanced);
    }

    Ok(StressSummary {
        case: "balanced-fanout",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus,
    })
}

fn run_percpu_fanout(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        let target = cpus[round % cpus.len()];
        let completed = completed.clone();
        let done = done.clone();
        let failures = failures.clone();
        let per_cpu = per_cpu.clone();
        spawn_on_cpu(target, "kwork-stress-percpu-producer", move || {
            let mut batch = Vec::new();
            for _ in 0..works {
                let completed = completed.clone();
                let per_cpu = per_cpu.clone();
                batch.push(ScheduledWork::new(move |_| {
                    record_current_cpu(&per_cpu);
                    completed.fetch_add(1, Ordering::AcqRel);
                }));
            }
            for work in &batch {
                match system_percpu_wq().queue_work_on(target, work) {
                    QueueWorkResult::Queued => {}
                    _ => {
                        failures.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }
            for work in &batch {
                if work.flush().is_err() {
                    failures.fetch_add(1, Ordering::AcqRel);
                }
            }
            done.fetch_add(1, Ordering::AcqRel);
        });
    }

    wait_until(|| done.load(Ordering::Acquire) >= rounds)?;
    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "percpu-fanout",
        rounds,
        queued: rounds * works,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_yield_cpu(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut queued = 0;

    for round in 0..rounds {
        let cpu_id = cpus[round % cpus.len()];
        let started = Arc::new(AtomicBool::new(false));
        let finish_long = Arc::new(AtomicBool::new(false));
        let long_started = started.clone();
        let long_finish = finish_long.clone();
        let long_completed = completed.clone();
        let long_per_cpu = per_cpu.clone();
        let long = ScheduledWork::new(move |_| {
            record_current_cpu(&long_per_cpu);
            long_started.store(true, Ordering::Release);
            while !long_finish.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            long_completed.fetch_add(1, Ordering::AcqRel);
        });
        let completed_ref = completed.clone();
        let other_per_cpu = per_cpu.clone();
        let other = ScheduledWork::new(move |_| {
            record_current_cpu(&other_per_cpu);
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });

        expect_queued(system_wq().queue_work_on(cpu_id, &long))?;
        queued += 1;
        wait_until(|| started.load(Ordering::Acquire))?;
        expect_queued(system_wq().queue_work_on(cpu_id, &other))?;
        queued += 1;
        let expected_completed = queued - 1;
        wait_until(|| completed.load(Ordering::Acquire) >= expected_completed)?;
        finish_long.store(true, Ordering::Release);
        long.flush().map_err(StressError::FlushFailed)?;
        other.flush().map_err(StressError::FlushFailed)?;
    }

    Ok(StressSummary {
        case: "yield-cpu",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_bh_drain(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let cpu_id = stress_cpu(SystemPoolKind::Bh)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut queued = 0;

    for _ in 0..rounds {
        let completed_ref = completed.clone();
        let per_cpu = per_cpu.clone();
        let work = ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu);
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });
        expect_queued(work.schedule_with(ScheduleAttrs::bottom_half().on_cpu(cpu_id)))?;
        queued += 1;
        wait_until(|| completed.load(Ordering::Acquire) >= queued)?;
    }

    Ok(StressSummary {
        case: "bh-drain",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_bh_highpri(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let cpu_id = stress_cpu(SystemPoolKind::Bh)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut queued = 0;

    for _ in 0..rounds {
        let completed_ref = completed.clone();
        let per_cpu = per_cpu.clone();
        let work = ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu);
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });
        expect_queued(system_bh_highpri_wq().queue_work_on(cpu_id, &work))?;
        queued += 1;
        wait_until(|| completed.load(Ordering::Acquire) >= queued)?;
    }

    Ok(StressSummary {
        case: "bh-highpri",
        rounds,
        queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_budgeted_poller(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let pending = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let poll_pending = pending.clone();
    let poll_completed = completed.clone();
    let poll_per_cpu = per_cpu.clone();
    let poller = BudgetedPoller::new(
        "kwork_stress_budgeted_poller",
        works.max(1),
        works.max(1),
        2,
        move |budget| {
            record_current_cpu(&poll_per_cpu);
            for _ in 0..budget {
                let mut current = poll_pending.load(Ordering::Acquire);
                loop {
                    if current == 0 {
                        return BudgetedPollProgress { has_more: false };
                    }
                    match poll_pending.compare_exchange(
                        current,
                        current - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => {
                            poll_completed.fetch_add(1, Ordering::AcqRel);
                            break;
                        }
                        Err(next) => current = next,
                    }
                }
            }
            BudgetedPollProgress {
                has_more: poll_pending.load(Ordering::Acquire) != 0,
            }
        },
    );
    poller
        .start()
        .map_err(|_| StressError::QueueFailed(QueueWorkResult::WorkerUnavailable))?;

    for round in 0..rounds {
        CURRENT_ROUND.store(round, Ordering::Release);
        CURRENT_PHASE.store(PHASE_BUDGETED_NOTIFY, Ordering::Release);
        pending.fetch_add(works, Ordering::AcqRel);
        let _ = poller.notify_irq_safe();
        if round % 2 == 0 {
            poller.assist_once();
        }
        CURRENT_PHASE.store(PHASE_BUDGETED_WAIT, Ordering::Release);
        wait_until(|| completed.load(Ordering::Acquire) >= (round + 1) * works)?;
    }

    CURRENT_PHASE.store(PHASE_BUDGETED_DESTROY, Ordering::Release);
    poller.destroy().map_err(StressError::FlushFailed)?;
    CURRENT_PHASE.store(PHASE_NONE, Ordering::Release);

    Ok(StressSummary {
        case: "budgeted-poller",
        rounds,
        queued: rounds * works,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_cancel_race(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let cancel_sync = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();
    let mut total_queued: usize = 0;

    for round in 0..rounds {
        CURRENT_ROUND.store(round, Ordering::Release);
        CURRENT_PHASE.store(PHASE_CANCEL_SPAWN, Ordering::Release);
        let target = cpus[round % cpus.len()];
        let cancel_cpu = cpus[(round + 1) % cpus.len()];
        let queued = Arc::new(AtomicUsize::new(0));
        let producer_done = Arc::new(AtomicBool::new(false));
        let canceller_done = Arc::new(AtomicBool::new(false));
        let batch = Arc::new(
            (0..works)
                .map(|_| {
                    let completed = completed.clone();
                    let per_cpu = per_cpu.clone();
                    ScheduledWork::new(move |_| {
                        record_current_cpu(&per_cpu);
                        ktask::yield_now();
                        completed.fetch_add(1, Ordering::AcqRel);
                    })
                })
                .collect::<Vec<_>>(),
        );

        let producer_batch = batch.clone();
        let producer_queued = queued.clone();
        let producer_failures = failures.clone();
        let producer_done_ref = producer_done.clone();
        spawn_on_cpu(target, "kwork-stress-cancel-producer", move || {
            for work in producer_batch.iter() {
                match system_wq().queue_work_on(target, work) {
                    QueueWorkResult::Queued => {
                        producer_queued.fetch_add(1, Ordering::AcqRel);
                    }
                    other => {
                        if !matches!(other, QueueWorkResult::AlreadyQueued) {
                            producer_failures.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                }
                ktask::yield_now();
            }
            producer_done_ref.store(true, Ordering::Release);
        });

        let canceller_batch = batch.clone();
        let canceller_queued = queued.clone();
        let canceller_cancel_sync = cancel_sync.clone();
        let canceller_failures = failures.clone();
        let canceller_done_ref = canceller_done.clone();
        spawn_on_cpu(cancel_cpu, "kwork-stress-cancel-canceller", move || {
            let _ = wait_until(|| canceller_queued.load(Ordering::Acquire) >= works / 2);
            for work in canceller_batch.iter() {
                match work.cancel_sync() {
                    Ok(true) => {
                        canceller_cancel_sync.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(false) => {}
                    Err(_) => {
                        canceller_failures.fetch_add(1, Ordering::AcqRel);
                    }
                }
                ktask::yield_now();
            }
            canceller_done_ref.store(true, Ordering::Release);
        });

        CURRENT_PHASE.store(PHASE_CANCEL_WAIT_TASKS, Ordering::Release);
        wait_until(|| {
            producer_done.load(Ordering::Acquire) && canceller_done.load(Ordering::Acquire)
        })?;
        total_queued = total_queued.saturating_add(queued.load(Ordering::Acquire));
        CURRENT_PHASE.store(PHASE_CANCEL_FINAL_FLUSH, Ordering::Release);
        for work in batch.iter() {
            work.flush().map_err(StressError::FlushFailed)?;
        }
    }

    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "cancel-race",
        rounds,
        queued: total_queued,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: cancel_sync.load(Ordering::Acquire),
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_disable_race(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let disabled = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        let target = cpus[round % cpus.len()];
        let toggle_cpu = cpus[(round + 1) % cpus.len()];
        let stop = Arc::new(AtomicBool::new(false));
        let completed_ref = completed.clone();
        let per_cpu_ref = per_cpu.clone();
        let work = Arc::new(ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu_ref);
            completed_ref.fetch_add(1, Ordering::AcqRel);
        }));

        let toggled_work = work.clone();
        let toggle_stop = stop.clone();
        let toggle_failures = failures.clone();
        let toggle_done = done.clone();
        spawn_on_cpu(toggle_cpu, "kwork-stress-disable-toggle", move || {
            while !toggle_stop.load(Ordering::Acquire) {
                if toggled_work.disable().is_err() {
                    toggle_failures.fetch_add(1, Ordering::AcqRel);
                    break;
                }
                ktask::yield_now();
                if toggled_work.enable().is_err() {
                    toggle_failures.fetch_add(1, Ordering::AcqRel);
                    break;
                }
                ktask::yield_now();
            }
            toggle_done.fetch_add(1, Ordering::AcqRel);
        });

        for _ in 0..works {
            match system_wq().queue_work_on(target, &work) {
                QueueWorkResult::Queued => {
                    work.flush().map_err(StressError::FlushFailed)?;
                }
                QueueWorkResult::Disabled => {
                    disabled.fetch_add(1, Ordering::AcqRel);
                }
                QueueWorkResult::AlreadyQueued => {
                    work.flush().map_err(StressError::FlushFailed)?;
                }
                other => return Err(StressError::QueueFailed(other)),
            }
            ktask::yield_now();
        }
        stop.store(true, Ordering::Release);
    }

    wait_until(|| done.load(Ordering::Acquire) >= rounds)?;
    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "disable-race",
        rounds,
        queued: rounds * works,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: disabled.load(Ordering::Acquire),
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_destroy_race(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let disabled = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        let target = cpus[round % cpus.len()];
        let destroy_cpu = cpus[(round + 1) % cpus.len()];
        let queue = WorkQueueHandle::alloc(
            "kwork_stress_destroy_race",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .map_err(|_| StressError::PoolNotReady)?;
        let queued = Arc::new(AtomicUsize::new(0));
        let batch = Arc::new(
            (0..works)
                .map(|_| {
                    let completed = completed.clone();
                    let per_cpu = per_cpu.clone();
                    ScheduledWork::new(move |_| {
                        record_current_cpu(&per_cpu);
                        ktask::yield_now();
                        completed.fetch_add(1, Ordering::AcqRel);
                    })
                })
                .collect::<Vec<_>>(),
        );

        let producer_batch = batch.clone();
        let producer_queued = queued.clone();
        let producer_disabled = disabled.clone();
        let producer_failures = failures.clone();
        let producer_done = done.clone();
        let producer_queue = queue.clone();
        spawn_on_cpu(target, "kwork-stress-destroy-producer", move || {
            for work in producer_batch.iter() {
                match producer_queue.queue_work_on(target, work) {
                    QueueWorkResult::Queued => {
                        producer_queued.fetch_add(1, Ordering::AcqRel);
                    }
                    QueueWorkResult::Disabled => {
                        producer_disabled.fetch_add(1, Ordering::AcqRel);
                    }
                    other => {
                        producer_failures.fetch_add(1, Ordering::AcqRel);
                        let _ = other;
                    }
                }
                ktask::yield_now();
            }
            producer_done.fetch_add(1, Ordering::AcqRel);
        });

        let destroy_queued = queued.clone();
        let destroy_failures = failures.clone();
        let destroy_done = done.clone();
        let destroy_queue = queue.clone();
        spawn_on_cpu(destroy_cpu, "kwork-stress-destroyer", move || {
            let _ = wait_until(|| destroy_queued.load(Ordering::Acquire) >= works / 2);
            if destroy_queue.destroy().is_err() {
                destroy_failures.fetch_add(1, Ordering::AcqRel);
            }
            destroy_done.fetch_add(1, Ordering::AcqRel);
        });
    }

    wait_until(|| done.load(Ordering::Acquire) >= rounds * 2)?;
    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "destroy-race",
        rounds,
        queued: rounds * works,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: disabled.load(Ordering::Acquire),
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_delayed_cancel(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let cancel_sync = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        let cpu_id = cpus[round % cpus.len()];
        let mut batch = Vec::new();
        for idx in 0..works {
            let completed = completed.clone();
            let per_cpu = per_cpu.clone();
            let work = DelayedScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu);
                completed.fetch_add(1, Ordering::AcqRel);
            });
            let attrs = ScheduleAttrs::system().on_cpu(cpu_id);
            match work.schedule_after_with(TimeSpan::from_millis(1), attrs) {
                QueueDelayedWorkResult::Queued => {}
                other => return Err(delayed_to_stress_error(other)),
            }
            if idx % 3 == 0 {
                if work.cancel_sync().map_err(StressError::FlushFailed)? {
                    cancel_sync.fetch_add(1, Ordering::AcqRel);
                }
            } else if idx % 3 == 1 {
                match work.mod_schedule_after_with(TimeSpan::ZERO, attrs) {
                    QueueDelayedWorkResult::Queued => {}
                    other => return Err(delayed_to_stress_error(other)),
                }
            }
            batch.push(work);
        }
        ktask::sleep(TimeSpan::from_millis(2));
        for work in &batch {
            if work.cancel_sync().map_err(StressError::FlushFailed)? {
                cancel_sync.fetch_add(1, Ordering::AcqRel);
            }
        }
    }

    Ok(StressSummary {
        case: "delayed-cancel",
        rounds,
        queued: rounds * works,
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: cancel_sync.load(Ordering::Acquire),
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_cancel_nonblocking(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let works = parse_arg(args, 1, DEFAULT_WORKS, 1, MAX_WORKS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let canceled = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        CURRENT_ROUND.store(round, Ordering::Release);
        let cpu_id = cpus[round % cpus.len()];
        let queue = WorkQueueHandle::alloc(
            "kwork_stress_cancel_nonblocking",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .map_err(|_| StressError::PoolNotReady)?;
        let blocker_started = Arc::new(AtomicBool::new(false));
        let blocker_finish = Arc::new(AtomicBool::new(false));
        let started = blocker_started.clone();
        let finish = blocker_finish.clone();
        let completed_ref = completed.clone();
        let per_cpu_ref = per_cpu.clone();
        let blocker = ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu_ref);
            started.store(true, Ordering::Release);
            while !finish.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });

        expect_queued(queue.queue_work_on(cpu_id, &blocker))?;
        wait_until(|| blocker_started.load(Ordering::Acquire))?;

        for idx in 0..works {
            let completed_ref = completed.clone();
            let per_cpu_ref = per_cpu.clone();
            let pending = ScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu_ref);
                completed_ref.fetch_add(1, Ordering::AcqRel);
            });
            expect_queued(queue.queue_work_on(cpu_id, &pending))?;
            match pending.cancel() {
                CancelWorkResult::CancelledPending => {
                    canceled.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    failures.fetch_add(1, Ordering::AcqRel);
                }
            }

            if idx % 4 == 0 {
                match blocker.cancel() {
                    CancelWorkResult::Running => {}
                    _ => {
                        failures.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }

            let completed_ref = completed.clone();
            let per_cpu_ref = per_cpu.clone();
            let delayed = DelayedScheduledWork::new(move |_| {
                record_current_cpu(&per_cpu_ref);
                completed_ref.fetch_add(1, Ordering::AcqRel);
            });
            match delayed.schedule_after_with(
                TimeSpan::from_millis(10),
                ScheduleAttrs::system().on_cpu(cpu_id),
            ) {
                QueueDelayedWorkResult::Queued => {}
                other => return Err(delayed_to_stress_error(other)),
            }
            match delayed.cancel() {
                CancelWorkResult::CancelledPending => {
                    canceled.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    failures.fetch_add(1, Ordering::AcqRel);
                }
            }
        }

        blocker_finish.store(true, Ordering::Release);
        blocker.flush().map_err(StressError::FlushFailed)?;
        queue.destroy().map_err(StressError::FlushFailed)?;
    }

    ktask::sleep(TimeSpan::from_millis(20));
    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "cancel-nonblocking",
        rounds,
        queued: rounds.saturating_mul(works).saturating_mul(2) + rounds,
        completed: completed.load(Ordering::Acquire),
        cancel: canceled.load(Ordering::Acquire),
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_wait_deadlock(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        CURRENT_ROUND.store(round, Ordering::Release);
        let cpu_id = cpus[round % cpus.len()];
        let queue = WorkQueueHandle::alloc(
            "kwork_stress_wait_deadlock",
            WorkQueueAttrs::new().with_max_active(1),
        )
        .map_err(|_| StressError::PoolNotReady)?;
        let completed_ref = completed.clone();
        let rejected_ref = rejected.clone();
        let failures_ref = failures.clone();
        let per_cpu_ref = per_cpu.clone();
        let probe_queue = queue.clone();
        let probe = ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu_ref);
            match probe_queue.flush() {
                Err(WorkqueueError::WouldDeadlock) => {
                    rejected_ref.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    failures_ref.fetch_add(1, Ordering::AcqRel);
                }
            }
            match probe_queue.destroy() {
                Err(WorkqueueError::WouldDeadlock) => {
                    rejected_ref.fetch_add(1, Ordering::AcqRel);
                }
                _ => {
                    failures_ref.fetch_add(1, Ordering::AcqRel);
                }
            }
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });

        expect_queued(queue.queue_work_on(cpu_id, &probe))?;
        probe.flush().map_err(StressError::FlushFailed)?;

        let completed_ref = completed.clone();
        let per_cpu_ref = per_cpu.clone();
        let after = ScheduledWork::new(move |_| {
            record_current_cpu(&per_cpu_ref);
            completed_ref.fetch_add(1, Ordering::AcqRel);
        });
        expect_queued(queue.queue_work_on(cpu_id, &after))?;
        after.flush().map_err(StressError::FlushFailed)?;
        queue.destroy().map_err(StressError::FlushFailed)?;
    }

    if failures.load(Ordering::Acquire) != 0
        || rejected.load(Ordering::Acquire) != rounds.saturating_mul(2)
    {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "wait-deadlock",
        rounds,
        queued: rounds.saturating_mul(2),
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: rejected.load(Ordering::Acquire),
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn run_sleep_block(args: &[&str]) -> Result<StressSummary, StressError> {
    let rounds = parse_arg(args, 0, DEFAULT_ROUNDS, 1, MAX_ROUNDS)?;
    let cpus = ready_cpus(SystemPoolKind::Normal)?;
    let completed = Arc::new(AtomicUsize::new(0));
    let failures = Arc::new(AtomicUsize::new(0));
    let per_cpu = per_cpu_counters();

    for round in 0..rounds {
        CURRENT_ROUND.store(round, Ordering::Release);
        let cpu_id = cpus[round % cpus.len()];
        let blocker_started = Arc::new(AtomicBool::new(false));
        let release_blocker = Arc::new(AtomicBool::new(false));
        let blocker_event = Arc::new(kpoll::PollEvent::new());
        let progress_runs = Arc::new(AtomicUsize::new(0));

        let started = blocker_started.clone();
        let release = release_blocker.clone();
        let event = blocker_event.clone();
        let blocker_per_cpu = per_cpu.clone();
        let blocker_completed = completed.clone();
        let blocker = ScheduledWork::new(move |_| {
            record_current_cpu(&blocker_per_cpu);
            started.store(true, Ordering::Release);
            if ktask::wait_for_poll_event_until(&event, || release.load(Ordering::Acquire)).is_ok()
            {
                blocker_completed.fetch_add(1, Ordering::AcqRel);
            }
        });

        let runs = progress_runs.clone();
        let release = release_blocker.clone();
        let event = blocker_event.clone();
        let progress_per_cpu = per_cpu.clone();
        let progress_completed = completed.clone();
        let progress = ScheduledWork::new(move |_| {
            record_current_cpu(&progress_per_cpu);
            runs.fetch_add(1, Ordering::AcqRel);
            release.store(true, Ordering::Release);
            event.notify();
            progress_completed.fetch_add(1, Ordering::AcqRel);
        });

        expect_queued(system_wq().queue_work_on(cpu_id, &blocker))?;
        wait_until(|| blocker_started.load(Ordering::Acquire))?;
        expect_queued(system_wq().queue_work_on(cpu_id, &progress))?;

        let progressed = wait_until_short(|| progress_runs.load(Ordering::Acquire) != 0);
        if !progressed {
            failures.fetch_add(1, Ordering::AcqRel);
            release_blocker.store(true, Ordering::Release);
            blocker_event.notify();
        }

        blocker.flush().map_err(StressError::FlushFailed)?;
        progress.flush().map_err(StressError::FlushFailed)?;
    }

    if failures.load(Ordering::Acquire) != 0 {
        return Err(StressError::Incomplete);
    }

    Ok(StressSummary {
        case: "sleep-block",
        rounds,
        queued: rounds.saturating_mul(2),
        completed: completed.load(Ordering::Acquire),
        cancel: 0,
        cancel_sync: 0,
        would_deadlock: 0,
        disabled: 0,
        failures: 0,
        active_cpus: active_cpu_count(&per_cpu),
    })
}

fn stress_cpu(kind: SystemPoolKind) -> Result<LogicalCpuId, StressError> {
    let preferred = runtime::current_cpu_id();
    if ensure_pool_ready(kind, preferred) {
        return Ok(preferred);
    }
    let fallback = LogicalCpuId::new(0);
    ensure_pool_ready(kind, fallback)
        .then_some(fallback)
        .ok_or(StressError::PoolNotReady)
}

fn ready_cpus(kind: SystemPoolKind) -> Result<Vec<LogicalCpuId>, StressError> {
    let mut cpus = Vec::new();
    for cpu in 0..kbuild_config::NR_CPUS {
        let cpu_id = LogicalCpuId::new(cpu);
        if builtinpool::is_system_worker_pool_ready(kind, cpu_id) {
            cpus.push(cpu_id);
        }
    }
    if cpus.is_empty() {
        let preferred = runtime::current_cpu_id();
        if ensure_pool_ready(kind, preferred) {
            cpus.push(preferred);
        }
    }
    (!cpus.is_empty())
        .then_some(cpus)
        .ok_or(StressError::PoolNotReady)
}

fn ensure_pool_ready(kind: SystemPoolKind, cpu_id: LogicalCpuId) -> bool {
    if builtinpool::is_system_worker_pool_ready(kind, cpu_id) {
        return true;
    }
    runtime::init_system_workqueue_worker_pools_for_cpu(cpu_id).is_some()
        && builtinpool::is_system_worker_pool_ready(kind, cpu_id)
}

fn expect_queued(result: QueueWorkResult) -> Result<(), StressError> {
    match result {
        QueueWorkResult::Queued => Ok(()),
        other => Err(StressError::QueueFailed(other)),
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) -> Result<(), StressError> {
    let deadline = ktask::monotonic_time() + WAIT_TIMEOUT;
    let mut spins = 0usize;
    loop {
        if ready() {
            return Ok(());
        }
        if ktask::monotonic_time() >= deadline {
            return Err(StressError::Timeout);
        }
        if spins < 64 {
            spins += 1;
            ktask::yield_now();
        } else {
            ktask::sleep(TimeSpan::from_millis(1));
        }
    }
}

fn wait_until_short(mut ready: impl FnMut() -> bool) -> bool {
    for _ in 0..100_000 {
        if ready() {
            return true;
        }
        ktask::yield_now();
    }
    false
}

fn per_cpu_counters() -> Arc<Vec<AtomicUsize>> {
    Arc::new(
        (0..kbuild_config::NR_CPUS)
            .map(|_| AtomicUsize::new(0))
            .collect(),
    )
}

fn record_current_cpu(counters: &[AtomicUsize]) {
    let cpu = runtime::current_cpu_id().as_usize();
    if let Some(counter) = counters.get(cpu) {
        counter.fetch_add(1, Ordering::AcqRel);
    }
}

fn active_cpu_count(counters: &[AtomicUsize]) -> usize {
    counters
        .iter()
        .filter(|counter| counter.load(Ordering::Acquire) != 0)
        .count()
}

fn spawn_on_cpu<F>(cpu_id: LogicalCpuId, name: &'static str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    ktask::spawn_with_name(
        move || {
            let _ = ktask::set_current_affinity(ktask::KCpuMask::one_shot_logical(cpu_id));
            f();
        },
        format!("{}/{}", name, cpu_id.as_usize()),
    );
}

fn delayed_to_stress_error(result: QueueDelayedWorkResult) -> StressError {
    match result {
        QueueDelayedWorkResult::Queued => StressError::Incomplete,
        QueueDelayedWorkResult::AlreadyQueued
        | QueueDelayedWorkResult::QueueFull
        | QueueDelayedWorkResult::Disabled
        | QueueDelayedWorkResult::InvalidCpu
        | QueueDelayedWorkResult::TimerUnavailable
        | QueueDelayedWorkResult::WorkerUnavailable => {
            StressError::QueueFailed(queue_work_result_from_delayed(result))
        }
    }
}

fn queue_work_result_from_delayed(result: QueueDelayedWorkResult) -> QueueWorkResult {
    match result {
        QueueDelayedWorkResult::Queued => QueueWorkResult::Queued,
        QueueDelayedWorkResult::AlreadyQueued => QueueWorkResult::AlreadyQueued,
        QueueDelayedWorkResult::QueueFull => QueueWorkResult::QueueFull,
        QueueDelayedWorkResult::Disabled => QueueWorkResult::Disabled,
        QueueDelayedWorkResult::InvalidCpu => QueueWorkResult::InvalidCpu,
        QueueDelayedWorkResult::TimerUnavailable | QueueDelayedWorkResult::WorkerUnavailable => {
            QueueWorkResult::WorkerUnavailable
        }
    }
}

fn parse_arg(
    args: &[&str],
    index: usize,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, StressError> {
    let Some(raw) = args.get(index) else {
        return Ok(default);
    };
    let value = raw
        .parse::<usize>()
        .map_err(|_| StressError::InvalidArgument)?;
    if value < min || value > max {
        return Err(StressError::InvalidArgument);
    }
    Ok(value)
}
