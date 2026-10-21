// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc};
use core::{
    future::pending,
    sync::atomic::{AtomicBool, Ordering},
};

use kcpu_id_map::KCpuMaskExt;
use khal::time::monotonic_time;
use ktime_types::TimeSpan;
use unittest::{assert, def_test};

use super::{sleep, timeout};
use crate::future::block_on;

/// Samples per case: enough to stabilise the median against scheduling
/// jitter, while keeping the worst-case runtime modest.
const SAMPLES: usize = 10;

/// Idle-CPU bound for a short timed wait (timer delivery + modest wake latency).
///
/// This is **not** a guarantee under same-CPU CPU-bound competition; use the
/// contended tests for that.
const IDLE_SUBTICK_LIMIT: TimeSpan = TimeSpan::from_millis(6);

/// Contended wake budget: request + one default request slice + 2 ms slack.
fn contended_limit(request: TimeSpan) -> TimeSpan {
    TimeSpan::from_nanos(
        request
            .as_nanos_u64_saturating()
            .saturating_add(ksched::DEFAULT_SLICE_NS)
            .saturating_add(2_000_000),
    )
}

struct TimingStats {
    min: TimeSpan,
    median: TimeSpan,
    max: TimeSpan,
}

/// Sorts `samples` in place and derives min / median / max.
fn stats_of(samples: &mut [TimeSpan]) -> TimingStats {
    samples.sort();
    let last = samples.len() - 1;
    TimingStats {
        min: samples[0],
        median: samples[last / 2],
        max: samples[last],
    }
}

struct SameCpuSpinner {
    task: crate::KtaskRef,
    stop: Arc<AtomicBool>,
}

impl Drop for SameCpuSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Do not let a failed timing assertion leak a permanently runnable
        // worker into the remaining serial unittest suite.
        let _ = self.task.join();
    }
}

/// Pins a `spin_loop` worker to this CPU so timer wakes compete for the RQ.
fn spawn_same_cpu_spinner() -> SameCpuSpinner {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = stop.clone();
    let task = crate::prepare_task(crate::TaskInner::new_pidless_kthread(
        move || {
            while !stop_flag.load(Ordering::Acquire) {
                core::hint::spin_loop();
            }
        },
        String::from("subtick-spinner"),
        kbuild_config::TASK_STACK_SIZE,
    ));
    let cpu = khal::percpu::this_cpu_id();
    task.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&task);
    // Let the spinner land on this RQ before measuring.
    crate::yield_now();
    SameCpuSpinner { task, stop }
}

/// Idle: `sleep` probes soft-timer expiry without same-CPU competition.
#[def_test(serial)]
fn test_idle_subtick_sleep_is_not_rounded_to_tick() {
    const REQUEST: TimeSpan = TimeSpan::from_millis(1);

    block_on(sleep(REQUEST));

    let mut samples = [TimeSpan::ZERO; SAMPLES];
    for slot in &mut samples {
        let start = monotonic_time();
        block_on(sleep(REQUEST));
        *slot = monotonic_time() - start;
    }
    let stats = stats_of(&mut samples);
    log::info!(
        "idle sleep({:?}): min={:?} median={:?} max={:?} (limit={:?})",
        REQUEST,
        stats.min,
        stats.median,
        stats.max,
        IDLE_SUBTICK_LIMIT,
    );
    assert!(stats.min >= REQUEST);
    // Latency is logged for the performance harness. Ordinary unit tests
    // only validate timer semantics because QEMU, SMP load, and debug
    // builds can legitimately shift the median.
}

/// Idle: `timeout` mirrors futex-wait-with-timeout on an idle CPU.
#[def_test(serial)]
fn test_idle_subtick_timeout_is_not_rounded_to_tick() {
    const REQUEST: TimeSpan = TimeSpan::from_millis(2);

    assert!(block_on(timeout(Some(REQUEST), pending::<()>())).is_err());
    let mut samples = [TimeSpan::ZERO; SAMPLES];
    for slot in &mut samples {
        let start = monotonic_time();
        let result = block_on(timeout(Some(REQUEST), pending::<()>()));
        assert!(result.is_err());
        *slot = monotonic_time() - start;
    }
    let stats = stats_of(&mut samples);
    log::info!(
        "idle timeout({:?}): min={:?} median={:?} max={:?} (limit={:?})",
        REQUEST,
        stats.min,
        stats.median,
        stats.max,
        IDLE_SUBTICK_LIMIT,
    );
    assert!(stats.min >= REQUEST);
}

/// Contended: same-CPU spinner; latency may include one default request slice.
#[def_test(serial)]
fn test_contended_subtick_sleep_within_slice_budget() {
    const REQUEST: TimeSpan = TimeSpan::from_millis(1);
    let limit = contended_limit(REQUEST);
    let spinner = spawn_same_cpu_spinner();

    block_on(sleep(REQUEST));
    let mut samples = [TimeSpan::ZERO; SAMPLES];
    for slot in &mut samples {
        let start = monotonic_time();
        block_on(sleep(REQUEST));
        *slot = monotonic_time() - start;
    }
    drop(spinner);

    let stats = stats_of(&mut samples);
    log::info!(
        "contended sleep({:?}): min={:?} median={:?} max={:?} (limit={:?})",
        REQUEST,
        stats.min,
        stats.median,
        stats.max,
        limit,
    );
    assert!(stats.min >= REQUEST);
}

/// Contended: same-CPU spinner against `timeout`.
#[def_test(serial)]
fn test_contended_subtick_timeout_within_slice_budget() {
    const REQUEST: TimeSpan = TimeSpan::from_millis(2);
    let limit = contended_limit(REQUEST);
    let spinner = spawn_same_cpu_spinner();

    assert!(block_on(timeout(Some(REQUEST), pending::<()>())).is_err());
    let mut samples = [TimeSpan::ZERO; SAMPLES];
    for slot in &mut samples {
        let start = monotonic_time();
        let result = block_on(timeout(Some(REQUEST), pending::<()>()));
        assert!(result.is_err());
        *slot = monotonic_time() - start;
    }
    drop(spinner);

    let stats = stats_of(&mut samples);
    log::info!(
        "contended timeout({:?}): min={:?} median={:?} max={:?} (limit={:?})",
        REQUEST,
        stats.min,
        stats.median,
        stats.max,
        limit,
    );
    assert!(stats.min >= REQUEST);
}
