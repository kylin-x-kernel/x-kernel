// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::KCpuMaskExt;
use unittest::def_test;

use super::*;
use crate::{
    queue::config::{CpuPolicy, QueueConfig},
    work::Work,
};

fn queue() -> Arc<Queue> {
    Queue::new(QueueConfig::new(&[0.into()], CpuPolicy::CurrentCpu, 8).unwrap())
}

fn wait_blocked(task: &ktask::KtaskRef) {
    while task.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
}

fn start_flush(queue: &Arc<Queue>) -> ktask::KtaskRef {
    let color = queue.current_color();
    let task = ktask::spawn({
        let queue = queue.clone();
        move || queue.flush().unwrap()
    });
    while queue.current_color() == color {
        ktask::yield_now();
    }
    wait_blocked(&task);
    task
}

/// One cutoff excludes later work and successive captures retain independent scope.
///
/// # Panics
/// Panics on early completion, waiting for later work or leaked queue ownership.
#[def_test]
fn independent_flushes_do_not_follow_new_acceptance() {
    let queue = queue();
    let first = core::pin::pin!(Work::with_callback(|| {}));
    let second = core::pin::pin!(Work::with_callback(|| {}));
    let later = core::pin::pin!(Work::with_callback(|| {}));
    // SAFETY: all pinned Work remains live until execution and joined drains below.
    unsafe { queue.submit(&first, None, || 0.into()) }.unwrap();
    let flush1 = start_flush(&queue);
    // SAFETY: pinned second Work outlives its execution and both flushes.
    unsafe { queue.submit(&second, None, || 0.into()) }.unwrap();
    let flush2 = start_flush(&queue);
    // SAFETY: later Work is explicitly canceled before leaving this scope.
    unsafe { queue.submit(&later, None, || 0.into()) }.unwrap();
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert_eq!(flush1.join(), 0);
    wait_blocked(&flush2);
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert_eq!(flush2.join(), 0);
    assert_eq!(later.observer_count(), 1);
    assert!(queue.admission_color().is_some());
    assert!(later.cancel_pending());
    queue.flush().unwrap();
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Exhausting sealed colors blocks only new capture, not submission or completion.
///
/// # Panics
/// Panics on a cutoff without a free color, lost wakeup or captured later work.
#[def_test]
fn full_ring_waits_for_completion_without_stopping_admission() {
    for first_completed in [0, 2] {
        let queue = queue();
        let works: Vec<_> = (0..5)
            .map(|_| Box::pin(Work::with_callback(|| {})))
            .collect();
        let mut flushes = Vec::new();
        for work in &works[..3] {
            // SAFETY: every pinned allocation survives cancellation and all joins.
            unsafe { queue.submit(work, None, || 0.into()) }.unwrap();
            flushes.push(start_flush(&queue));
        }
        let full_color = queue.current_color();
        let overflow = ktask::spawn({
            let queue = queue.clone();
            move || queue.flush().unwrap()
        });
        wait_blocked(&overflow);
        assert_eq!(queue.current_color(), full_color);
        // SAFETY: this prepared Work is canceled before storage is released.
        unsafe { queue.submit(&works[3], None, || 0.into()) }.unwrap();
        assert!(works[first_completed].cancel_pending());
        while queue.current_color() == full_color {
            ktask::yield_now();
        }
        wait_blocked(&overflow);
        // SAFETY: retained pinned storage outlives the final cancellation below.
        unsafe { queue.submit(&works[4], None, || 0.into()) }.unwrap();
        for (index, work) in works[..4].iter().enumerate() {
            if index == first_completed {
                continue;
            }
            assert!(work.cancel_pending());
        }
        for flush in flushes {
            assert_eq!(flush.join(), 0);
        }
        assert_eq!(overflow.join(), 0);
        assert_eq!(works[4].observer_count(), 1);
        assert!(works[4].cancel_pending());
    }
}

/// Shutdown drains independently of both full color space and the flush mutex.
///
/// # Panics
/// Panics on failed close/drain, lost work or inadvertent reopening by flush.
#[def_test]
fn shutdown_needs_neither_color_space_nor_flush_serializer() {
    let queue = queue();
    let works: Vec<_> = (0..3)
        .map(|_| Box::pin(Work::with_callback(|| {})))
        .collect();
    let mut flushes = Vec::new();
    for work in &works {
        // SAFETY: all accepted Work is canceled before storage and queue release.
        unsafe { queue.submit(work, None, || 0.into()) }.unwrap();
        flushes.push(start_flush(&queue));
    }
    let control = queue.sealed_colors.lock();
    let drain = ktask::spawn({
        let queue = queue.clone();
        move || queue.close_and_drain().unwrap()
    });
    while queue.admission_color().is_some() {
        ktask::yield_now();
    }
    for work in &works {
        assert!(work.cancel_pending());
    }
    assert_eq!(drain.join(), 0);
    drop(control);
    for flush in flushes {
        assert_eq!(flush.join(), 0);
    }
    queue.flush().unwrap();
    assert!(queue.admission_color().is_none());
}

/// An old receipt and a later flush cannot be redirected to a recycled color.
///
/// # Panics
/// Panics on premature return, revived receipt or accidental wait for reused work.
#[def_test]
fn reused_color_does_not_extend_retained_old_receipts() {
    let queue = queue();
    let old = core::pin::pin!(Work::with_callback(|| {}));
    let held = core::pin::pin!(Work::with_callback(|| {}));
    let replacement = core::pin::pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work is retained through cancellation and all flush joins.
    unsafe { queue.submit(&old, None, || 0.into()) }.unwrap();
    let first_color = queue.current_color();
    let flush1 = start_flush(&queue);
    let old_receipt = queue.sealed_colors.lock()[first_color]
        .as_ref()
        .unwrap()
        .clone();
    // SAFETY: held Work is canceled before its storage leaves scope.
    unsafe { queue.submit(&held, None, || 0.into()) }.unwrap();
    let flush2 = start_flush(&queue);
    assert!(old.cancel_pending());
    assert_eq!(flush1.join(), 0);
    assert!(old_receipt.is_completed());
    let mut flushes = Vec::new();
    while queue.current_color() != first_color {
        flushes.push(start_flush(&queue));
    }
    // SAFETY: pinned replacement is canceled after all older flushes return.
    unsafe { queue.submit(&replacement, None, || 0.into()) }.unwrap();
    assert!(old_receipt.is_completed());
    assert!(held.cancel_pending());
    assert_eq!(flush2.join(), 0);
    for flush in flushes {
        assert_eq!(flush.join(), 0);
    }
    assert_eq!(replacement.observer_count(), 1);
    assert!(replacement.cancel_pending());
}

/// Partition barriers observe late commits of an already sampled old color.
///
/// # Panics
/// Panics if an old-color participation escapes the cutoff or a CPU is omitted.
#[def_test]
fn flush_visits_all_partitions_after_common_cutoff() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let queue =
        Queue::new(QueueConfig::new(&[0.into(), 1.into()], CpuPolicy::CurrentCpu, 1).unwrap());
    let late = Arc::new(Work::new());
    let published = Arc::new(AtomicBool::new(false));
    let flusher = ktask::spawn({
        let (queue, late, published) = (queue.clone(), late.clone(), published.clone());
        move || {
            queue
                .flush_with(
                    || {},
                    |old| {
                        // Models a pre-cutoff sample committing before the last CPU barrier.
                        // SAFETY: retained Arc is stable and stays live until cancellation/join.
                        unsafe { queue.partitions[1].accept(&late, (), old) }.unwrap();
                        published.store(true, Ordering::Release);
                    },
                )
                .unwrap()
        }
    });
    while !published.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    wait_blocked(&flusher);
    assert!(late.cancel_pending());
    assert_eq!(flusher.join(), 0);
}

/// A real remote acceptor holds the partition barrier across color publication.
///
/// # Panics
/// Panics on incorrect CPU placement, premature flush return, inclusion of later
/// work, failed drain or leaked Queue ownership.
#[def_test]
fn remote_sample_before_cutoff_commits_after_publication() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let queue =
        Queue::new(QueueConfig::new(&[0.into(), 1.into()], CpuPolicy::CurrentCpu, 2).unwrap());
    let old = Arc::new(Work::with_callback(|| {}));
    let later = core::pin::pin!(Work::with_callback(|| {}));
    for round in 0..32 {
        let begin = Arc::new(AtomicBool::new(false));
        let sampled = Arc::new(AtomicBool::new(false));
        let cpu = (round % 2).into();
        let producer = ktask::spawn({
            let (queue, old, begin, sampled) =
                (queue.clone(), old.clone(), begin.clone(), sampled.clone());
            move || {
                assert!(ktask::set_current_affinity(
                    ktask::KCpuMask::one_shot_logical(1.into())
                ));
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), 1);
                while !begin.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
                let partition = queue.partition(cpu).unwrap();
                assert!(
                    // SAFETY: outer Arc retains stable shared Work until this task and
                    // flush join, after explicit cancellation. Queue owns the partition.
                    unsafe {
                        partition.accept_using(&old, queue.clone(), || {
                            let color = queue.admission_color().unwrap();
                            sampled.store(true, Ordering::Release);
                            // Test-only pause under the real admission barrier. CPU0 can
                            // publish without this lock; never park/yield while holding it.
                            while queue.current_color() == color {
                                core::hint::spin_loop();
                            }
                            Some(color)
                        })
                    }
                    .is_ok()
                );
            }
        });
        let flusher = ktask::spawn({
            let (queue, begin, sampled) = (queue.clone(), begin.clone(), sampled.clone());
            move || {
                assert!(ktask::set_current_affinity(
                    ktask::KCpuMask::one_shot_logical(0.into())
                ));
                assert_eq!(khal::percpu::this_cpu_id().as_usize(), 0);
                queue
                    .flush_with(
                        || {
                            begin.store(true, Ordering::Release);
                            while !sampled.load(Ordering::Acquire) {
                                ktask::yield_now();
                            }
                        },
                        |_| {},
                    )
                    .unwrap();
            }
        });
        assert_eq!(producer.join(), 0);
        loop {
            match flusher.state() {
                ktask::TaskState::Blocked => break,
                ktask::TaskState::Exited => panic!("flush missed the old-color acceptance"),
                _ => ktask::yield_now(),
            }
        }
        // SAFETY: pinned later Work remains live and is canceled after the old flush joins.
        unsafe { queue.submit(&later, Some(cpu), || unreachable!()) }.unwrap();
        assert!(old.cancel_pending());
        assert_eq!(flusher.join(), 0);
        assert_eq!(later.observer_count(), 1);
        assert!(later.cancel_pending());
        assert_eq!(old.observer_count(), 0);
        assert_eq!(Arc::strong_count(&queue), 1);
    }
    queue.close_and_drain().unwrap();
}

/// Invalid context preserves the current acceptance color.
///
/// # Panics
/// Panics on preflight effects or accepted invalid context.
#[def_test]
fn flush_errors_precede_cutoff_and_idle_flush_preserves_closure() {
    let queue = queue();
    let current = queue.current_color();
    assert!(queue.sealed_colors.lock().iter().all(Option::is_none));
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(queue.flush(), Err(WaitError::InvalidContext));
    }
    assert_eq!(queue.current_color(), current);
    for _ in 0..12 {
        queue.flush().unwrap();
    }
    queue.close_and_drain().unwrap();
    for _ in 0..12 {
        queue.flush().unwrap();
        assert!(queue.admission_color().is_none());
    }
}

/// Own-queue callback diagnosis runs before any color or accounting changes.
///
/// # Panics
/// Panics on self-wait acceptance or changed cutoff after rejection.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn callback_flush_rejection_preserves_cutoff() {
    let queue = queue();
    let work = core::pin::pin!(Work::with_callback(|| {
        let color = queue.current_color();
        assert_eq!(queue.flush(), Err(WaitError::SelfWait));
        assert_eq!(queue.current_color(), color);
    }));
    // SAFETY: pinned Work and captures survive the controlled execution and drain.
    unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
    queue.claim_with_demand(0.into()).0.unwrap().run();
    queue.flush().unwrap();
}

struct Resource(Option<Box<dyn FnOnce() + Send>>);

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

/// Already detached cancellation cleanup keeps its original color unavailable.
///
/// # Panics
/// Panics on early flush completion, lost cleanup accounting or incomplete joins.
#[def_test]
fn flush_includes_cleanup_detached_before_capture() {
    let queue = queue();
    let work = Arc::new(Work::new());
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let resource = Resource(Some(Box::new({
        let (started, release) = (started.clone(), release.clone());
        move || {
            started.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                ktask::yield_now();
            }
        }
    })));
    // SAFETY: retained stable Arc survives cancellation cleanup and joined flush.
    assert!(unsafe { queue.partitions[0].accept(&work, resource, queue.current_color()) }.is_ok());
    let cancel = ktask::spawn({
        let work = work.clone();
        move || assert!(work.cancel_pending())
    });
    while !started.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    assert_eq!(work.observer_count(), 0);
    let color = queue.current_color();
    let flush = start_flush(&queue);
    assert!(
        !queue.sealed_colors.lock()[color]
            .as_ref()
            .unwrap()
            .is_completed()
    );
    release.store(true, Ordering::Release);
    assert_eq!(cancel.join(), 0);
    assert_eq!(flush.join(), 0);
}

/// Invalid context preserves both cutoff and an older live receipt.
#[def_test]
fn older_receipt_survives_rejected_capture() {
    let queue = queue();
    let work = core::pin::pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work remains live through cancellation and the joined flush.
    unsafe { queue.submit(&work, None, || 0.into()) }.unwrap();
    let first_color = queue.current_color();
    let first = start_flush(&queue);
    let original = queue.sealed_colors.lock()[first_color]
        .as_ref()
        .unwrap()
        .clone();
    let current = queue.current_color();
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(queue.flush(), Err(WaitError::InvalidContext));
    }
    assert_eq!(queue.current_color(), current);
    assert!(Arc::ptr_eq(
        queue.sealed_colors.lock()[first_color].as_ref().unwrap(),
        &original
    ));
    assert!(!original.is_completed());
    assert!(work.cancel_pending());
    assert_eq!(first.join(), 0);
    queue.flush().unwrap();
    assert_eq!(Arc::strong_count(&original), 1);
}

/// A competing capture before setup leaves each flush with exactly one cutoff.
#[def_test]
fn competing_capture_preserves_cutoff_identity() {
    let queue = queue();
    queue.flush().unwrap();
    let before = queue.current_color();
    let mut publications = 0;
    queue
        .flush_with(|| queue.flush().unwrap(), |_| publications += 1)
        .unwrap();
    assert_eq!(publications, 1);
    assert_eq!(queue.current_color(), (before + 2) % COLOR_COUNT);
}
