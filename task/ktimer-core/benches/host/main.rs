// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Measures native timer-core operation costs and checks owned-storage lifetimes.
//!
//! This executable compiles the timer-core sources with real `kspin` atomic
//! locking and no-op host context guards. It reports operation timings and
//! allocation counts; it does not measure kernel IRQ, IPI, scheduler or futex
//! latency. Its supported entry point is the command line, not a library API.
//! Start with `--check-owned` for ownership and allocation checks, then use
//! `run.sh measure` or `run.sh compare` for coordinated performance measurements.
//!
//! # Examples
//!
//! From the repository root, build offline with the pinned toolchain and run
//! the ownership checks (no kernel configuration or QEMU is required):
//!
//! ```sh
//! BENCH_TARGET=/tmp/timer-core-bench-target bash task/ktimer-core/benches/host/run.sh build
//! /tmp/timer-core-bench-target/release/timer-core-bench --check-owned
//! ```
//!
//! A successful check exits with status zero and reports allocation/free counts
//! for timer operations, final storage release and the allocator probe control.
//! See `task/ktimer-core/benches/host/README.md`
//! for workload selection, measurement commands and interpretation limits.

#![allow(dead_code)]
#[cfg(any(not(feature = "smp"), feature = "stats"))]
compile_error!("benchmark requires smp and does not support lock statistics");

extern crate alloc;
extern crate self as karch;
extern crate self as kcpu_id_map;
extern crate self as kspin;

#[path = "../../../../arch/karch/src/cache.rs"]
pub mod cache;
mod host;
pub use host::{BaseGuard, LogicalCpuId};
mod guard {
    pub use super::BaseGuard;
}
#[path = "../../../kspin/src/guard/types.rs"]
mod guard_types;
pub use guard_types::{IrqSave, NoOp, NoPreempt, NoPreemptIrqSave};
#[path = "../../../kspin/src/lock.rs"]
mod lock;
/// Host spinlock alias with no-op IRQ and preemption guards.
pub type SpinNoIrq<T> = lock::SpinLock<NoPreemptIrqSave, T>;
/// RAII guard that unlocks the host spinlock without changing IRQ or preemption state.
pub type SpinNoIrqGuard<'a, T> = lock::SpinLockGuard<'a, NoPreemptIrqSave, T>;

#[path = "../../src/control.rs"]
mod control;
#[path = "../../src/cpu.rs"]
mod cpu;
#[path = "../../src/deadline.rs"]
mod deadline;
#[path = "../../src/dispatch.rs"]
mod dispatch;
#[path = "../../src/index.rs"]
mod index;
#[path = "../../src/set.rs"]
mod set;
#[path = "../../src/timer.rs"]
mod timer;

mod comparison;
mod cpu_slots;
mod owned_checks;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};

use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{
    control::TimerControl,
    deadline::{Deadline, Expiration},
    index::{TimerHandler, TimerNode},
    set::TimerSet,
};

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);
struct CountingAllocator;

// SAFETY: every allocation operation is forwarded to System with unchanged
// pointer/layout arguments. Counters use only atomics and cannot allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded allocator contract and original layout.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if COUNTING.load(Ordering::Relaxed) {
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: pointer and matching layout originate from System.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            FREES.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: forwarded pointer, original layout and requested new size.
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

struct Handler(AtomicUsize);
impl TimerHandler for Handler {
    fn on_expiration(&self, _: TimerControl<'_>, event: Expiration) {
        black_box(event);
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn setting(deadline: u64) -> Deadline {
    Deadline::new(MonotonicInstant::from_span_since_origin(
        TimeSpan::from_nanos(deadline),
    ))
    .unwrap()
}

struct Sample {
    operations: usize,
    elapsed_ns: u128,
    callbacks: usize,
    allocs: usize,
    frees: usize,
}

fn sample(case: &str, n: usize, events: usize) -> Sample {
    let handler = Box::new(Handler(AtomicUsize::new(0)));
    let nodes: Box<[TimerNode]> = (0..n)
        .map(|_| {
            // SAFETY: handler lives in a stable Box until after every node is drained.
            unsafe { TimerNode::with_handler(setting(1_000), &*handler) }
        })
        .collect();
    let timers = TimerSet::<1>::new();
    let arm_all = || {
        for (i, node) in nodes.iter().enumerate() {
            let deadline = if case == "dispatch" {
                1_000
            } else {
                1_000 + i as u64
            };
            // SAFETY: fixed boxed nodes and handler outlive this stable set;
            // one thread performs all operations and drains it before release.
            unsafe { timers.arm_on(node, 0.into(), setting(deadline)) }.unwrap();
        }
    };
    ALLOCS.store(0, Ordering::Relaxed);
    FREES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let mut elapsed_ns = 0;
    let operations;
    if case == "rearm" || case == "cancel_arm" {
        arm_all();
        let start = Instant::now();
        for i in 0..events {
            let node = &nodes[i.wrapping_mul(73) & (n - 1)];
            if case == "cancel_arm" {
                // SAFETY: matching live node/set; no callback is running.
                assert!(unsafe { timers.cancel_sync(node) });
            }
            // SAFETY: same external storage and serialized owner protocol.
            black_box(
                unsafe { timers.arm_on(node, 0.into(), setting(1_000_000 + i as u64)) }.unwrap(),
            );
        }
        elapsed_ns = start.elapsed().as_nanos();
        operations = events;
    } else {
        let batches = (events / n).max(1);
        operations = batches * n;
        for _ in 0..batches {
            arm_all();
            let now = 1_000;
            let start = Instant::now();
            // SAFETY: exclusive simulated CPU0 service, live external storage,
            // fixed nondecreasing clock per delivery and bounded counted handler.
            assert!(unsafe { timers.dispatch_due(0.into(), || black_box(now)) }.unwrap());
            elapsed_ns += start.elapsed().as_nanos();
        }
    }
    let mut removed = 0;
    for node in &nodes {
        // SAFETY: no callback remains running; remove every successor before
        // node/handler storage is dropped. Cleanup is counted but not timed.
        removed += usize::from(unsafe { timers.cancel_sync(node) });
    }
    COUNTING.store(false, Ordering::Relaxed);
    let callbacks = handler.0.load(Ordering::Relaxed);
    let allocs = ALLOCS.load(Ordering::Relaxed);
    let frees = FREES.load(Ordering::Relaxed);
    assert_eq!(removed, if case == "dispatch" { 0 } else { n });
    assert_eq!(callbacks, if case == "dispatch" { operations } else { 0 });
    assert!(nodes.iter().all(|node| !node.is_linked()));
    assert_eq!(
        (allocs, frees),
        (0, 0),
        "operation phase must not allocate or free"
    );
    Sample {
        operations,
        elapsed_ns,
        callbacks,
        allocs,
        frees,
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next();
    if first.as_deref() == Some("--cpu-slots") {
        let cpus = core::array::from_fn(|_| {
            args.next()
                .expect("two host CPUs required")
                .parse()
                .unwrap()
        });
        let iterations = args
            .next()
            .expect("iteration count required")
            .parse()
            .unwrap();
        assert!(args.next().is_none(), "--cpu-slots CPU0 CPU1 ITERATIONS");
        cpu_slots::run(cpus, iterations);
        return;
    }
    if first.as_deref() == Some("--compare-sleep-state") {
        let events = args.next().map(|x| x.parse().unwrap()).unwrap_or(65536);
        assert!(
            args.next().is_none(),
            "--compare-sleep-state takes only an event count"
        );
        comparison::compare_sleep_state(events);
        return;
    }
    if first.as_deref() == Some("--profile-lifecycle") {
        let events = args.next().map(|x| x.parse().unwrap()).unwrap_or(65536);
        assert!(
            args.next().is_none(),
            "--profile-lifecycle takes only an event count"
        );
        comparison::profile(events);
        return;
    }
    if first.as_deref() == Some("--compare") {
        let events = args.next().map(|x| x.parse().unwrap()).unwrap_or(65536);
        assert!(args.next().is_none(), "--compare takes only an event count");
        comparison::run(events);
        return;
    }
    if first.as_deref() == Some("--check-owned") {
        assert!(args.next().is_none(), "--check-owned takes no arguments");
        owned_checks::run();
        return;
    }
    let rounds: usize = first.map(|x| x.parse().unwrap()).unwrap_or(5);
    let events: usize = args.next().map(|x| x.parse().unwrap()).unwrap_or(200_000);
    assert!((1..=100).contains(&rounds) && (4096..=10_000_000).contains(&events));
    assert!(
        args.next().is_none(),
        "usage: timer-core-bench [rounds] [events]"
    );
    println!("case,n,round,operations,elapsed_ns,ns_per_operation,allocs,frees,callbacks");
    for n in [1, 32, 256, 4096] {
        for case in ["rearm", "cancel_arm", "dispatch"] {
            black_box(sample(case, n, 4096));
            for round in 0..rounds {
                let s = sample(case, n, events);
                println!(
                    "{case},{n},{round},{},{},{:.3},{},{},{}",
                    s.operations,
                    s.elapsed_ns,
                    s.elapsed_ns as f64 / s.operations as f64,
                    s.allocs,
                    s.frees,
                    s.callbacks
                );
            }
        }
    }
}
