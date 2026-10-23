// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Public drain contracts against boot's permanent RCU service. Fixtures join
//! their downstream tasks without replacing service state or callback carriers.

use alloc::{boxed::Box, sync::Arc};
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use kcpu_id_map::KCpuMaskExt;
use kerrno::KError;
use krcu::{CallbackNode, PreparedCallback, drain_callbacks};
use ksync::Mutex;
use ktask::future::PreparedTaskWait;
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn drain_rejects_masked_and_read_scope_contexts() {
    let masked = {
        let _guard = kspin::IrqSave::new();
        // SAFETY: no callback-dependent resource is held; invalid wait context
        // must be rejected before acquiring the drain mutex or publishing nodes.
        unsafe { drain_callbacks() }
    };
    assert_eq!(masked, Err(KError::InvalidInput));
    let in_scope = krcu::with_read(|_| {
        // SAFETY: this fixture withdraws no object and only tests rejection of
        // an active read scope, before any drain can be installed.
        unsafe { drain_callbacks() }
    });
    assert_eq!(in_scope, Err(KError::InvalidInput));
}

#[repr(C)]
struct DownstreamProbe {
    node: CallbackNode,
    task: ktask::KtaskRef,
}

const _: () = core::assert!(core::mem::offset_of!(DownstreamProbe, node) == 0);

/// # Safety
/// `node` projects the first field of a uniquely accepted DownstreamProbe Box.
/// Its prepared task has not been activated; invocation runs outside RCU locks.
unsafe fn activate_downstream(node: *mut CallbackNode) {
    // SAFETY: the first-field projection preserves the original Box allocation.
    let probe = unsafe { Box::from_raw(node.cast::<DownstreamProbe>()) };
    ktask::activate_task(&probe.task);
}

/// # Safety
/// `node` is the first-field projection of the unsubmitted Box owned by the
/// prepared handle. No reader can access it; cancellation consumes it once.
unsafe fn cancel_downstream(node: *mut CallbackNode) {
    // SAFETY: repr(C), the offset assertion and unique cancellation ownership
    // preserve the Box base. Cancellation must not activate the prepared task.
    drop(unsafe { Box::from_raw(node.cast::<DownstreamProbe>()) });
}

#[def_test(serial)]
fn callback_drain_does_not_wait_for_downstream_task_work() {
    let gate = Arc::new(Mutex::new(()));
    let worker_gate = gate.clone();
    let finished = Arc::new(AtomicBool::new(false));
    let worker_finished = finished.clone();
    let worker = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let _guard = worker_gate.lock();
            worker_finished.store(true, Ordering::Release);
        },
        "rcu-downstream".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    let blocked = gate.lock();
    let record = Box::into_raw(Box::new(DownstreamProbe {
        node: CallbackNode::new(activate_downstream),
        task: worker.clone(),
    }));
    // SAFETY: project from the whole uniquely retained Box.
    let node = NonNull::new(unsafe { &raw mut (*record).node }).unwrap();
    // SAFETY: unpublished stable Box transfers its sole submission right. Both
    // paths consume it once; cancellation only drops the retained task owner.
    let prepared = unsafe { PreparedCallback::new(node, cancel_downstream) };
    // SAFETY: unpublished fixture with a preallocated task; callback only
    // activates it and never waits for the downstream gate held by this test.
    let accepted = match unsafe { prepared.submit() } {
        Ok(()) => true,
        Err((_, prepared)) => {
            drop(prepared);
            // Ensure cleanup/join also completes on a rejected admission.
            ktask::activate_task(&worker);
            false
        }
    };
    // SAFETY: the held gate is used only by independent downstream work, never
    // by the callback or GP service. The sole producer has stopped.
    let drained = unsafe { drain_callbacks() };
    let finished_before_release = finished.load(Ordering::Acquire);
    drop(blocked);
    let status = worker.join();
    assert!(accepted);
    assert_eq!(drained, Ok(()));
    assert!(!finished_before_release);
    assert_eq!(status, 0);
    assert!(finished.load(Ordering::Acquire));
}

#[repr(C)]
struct DrainProbe {
    node: CallbackNode,
    destroyed: Arc<AtomicUsize>,
}

const _: () = core::assert!(core::mem::offset_of!(DrainProbe, node) == 0);

impl Drop for DrainProbe {
    fn drop(&mut self) {
        self.destroyed.fetch_add(1, Ordering::Release);
    }
}

/// # Safety
/// `node` projects the first field of a uniquely transferred DrainProbe Box.
/// The probe has no published readers; invocation or cancellation consumes
/// that Box once outside queue locks, without requiring a GP for cancellation.
unsafe fn destroy_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the original Box base.
    drop(unsafe { Box::from_raw(node.cast::<DrainProbe>()) });
}

fn prepare_drain_probe(destroyed: &Arc<AtomicUsize>) -> PreparedCallback {
    let record = Box::into_raw(Box::new(DrainProbe {
        node: CallbackNode::new(destroy_probe),
        destroyed: destroyed.clone(),
    }));
    // SAFETY: project from the whole uniquely owned Box allocation.
    let node = NonNull::new(unsafe { &raw mut (*record).node }).unwrap();
    // SAFETY: unpublished stable storage transfers its only submission right.
    // Invocation and cancellation both release this reader-free Box exactly once.
    unsafe { PreparedCallback::new(node, destroy_probe) }
}

#[def_test(serial)]
fn drain_waits_for_every_cpu_and_reuses_markers_across_batches() {
    // Fixed public workload for completion and marker reuse, not a copy of
    // the private executor budget. krcu tests its own per-pass policy.
    const PER_CPU: usize = 65;
    let destroyed = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::new(AtomicUsize::new(0));
    let has_wrong_cpu = Arc::new(AtomicBool::new(false));
    for batch in 1..=3 {
        for cpu in 0..kcpu_id_map::nr_cpus() {
            let destroyed = destroyed.clone();
            let accepted = accepted.clone();
            let has_wrong_cpu = has_wrong_cpu.clone();
            let worker = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
                move || {
                    let prepared: alloc::vec::Vec<_> = (0..PER_CPU)
                        .map(|_| prepare_drain_probe(&destroyed))
                        .collect();
                    // Keep the entire burst queued on the actual bound CPU
                    // before its softirq executor can consume the first batch.
                    let _guard = kspin::NoPreemptIrqSave::new();
                    if khal::percpu::this_cpu_id().as_usize() != cpu {
                        has_wrong_cpu.store(true, Ordering::Release);
                    }
                    for callback in prepared {
                        // SAFETY: unpublished probes, nonblocking destruction,
                        // watched pinned CPU and no fixture or scheduler lock.
                        match unsafe { callback.submit() } {
                            Ok(()) => {
                                accepted.fetch_add(1, Ordering::Release);
                            }
                            Err((_, callback)) => drop(callback),
                        }
                    }
                },
                "rcu-drain-producer".into(),
                kbuild_config::TASK_STACK_SIZE,
            ));
            worker.set_cpumask(ktask::KCpuMask::one_shot_logical(cpu.into()));
            ktask::activate_task(&worker);
            let status = worker.join();
            assert_eq!(status, 0);
        }
        // SAFETY: all producers have joined, no callback-needed resource is
        // held, and boot retains the service and every participating CPU.
        let drained = unsafe { drain_callbacks() };
        assert_eq!(drained, Ok(()));
        assert!(!has_wrong_cpu.load(Ordering::Acquire));
        assert_eq!(
            accepted.load(Ordering::Acquire),
            batch * kcpu_id_map::nr_cpus() * PER_CPU
        );
        assert_eq!(
            destroyed.load(Ordering::Acquire),
            accepted.load(Ordering::Acquire)
        );
        assert_eq!(Arc::strong_count(&destroyed), 1);
    }
    for _ in 0..3 {
        // SAFETY: the same stopped producers and resource-free task context.
        assert_eq!(unsafe { drain_callbacks() }, Ok(()));
    }
}

#[def_test(serial)]
fn concurrent_drains_serialize_and_observe_completed_destruction() {
    let results = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let destroyed: [_; 2] = core::array::from_fn(|_| Arc::new(AtomicUsize::new(0)));
    let mut observations = None;
    krcu::host::RcuTaskIf::with_serialized_drain(&mut || {
        let tasks: [_; 2] = core::array::from_fn(|id| {
            let results = results.clone();
            let own_destroyed = destroyed[id].clone();
            ktask::spawn(move || {
                let callback = prepare_drain_probe(&own_destroyed);
                // SAFETY: unique unpublished fixture with nonblocking cleanup;
                // the task holds no callback-needed resource.
                let accepted = match unsafe { callback.submit() } {
                    Ok(()) => true,
                    Err((_, callback)) => {
                        drop(callback);
                        false
                    }
                };
                // No operation after this stage can park before drain's mutex.
                results[id].store(1, Ordering::Release);
                // SAFETY: production stopped; all service CPUs remain live and
                // this worker holds no lock or read scope needed for progress.
                let drained = unsafe { drain_callbacks() }.is_ok();
                let is_complete = accepted
                    && drained
                    && own_destroyed.load(Ordering::Acquire) == 1
                    && Arc::strong_count(&own_destroyed) == 2;
                results[id].store(if is_complete { 2 } else { 3 }, Ordering::Release);
            })
        });
        for (id, task) in tasks.iter().enumerate() {
            while results[id].load(Ordering::Acquire) == 0
                || (results[id].load(Ordering::Acquire) == 1
                    && task.state() != ktask::TaskState::Blocked)
            {
                ktask::yield_now();
            }
        }
        // Sample before waiting for callbacks: completion could otherwise
        // erase evidence of markers installed before acquiring the host gate.
        let has_installed_marker = krcu::has_installed_drain_markers_for_tests();
        while destroyed
            .iter()
            .any(|count| count.load(Ordering::Acquire) == 0)
        {
            ktask::yield_now();
        }
        // Callback progress cannot let either drain bypass serialization.
        // Private marker ownership/reuse is checked separately inside krcu.
        observations = Some((
            tasks,
            has_installed_marker,
            core::array::from_fn::<_, 2, _>(|id| results[id].load(Ordering::Acquire)),
            destroyed
                .iter()
                .map(|count| count.load(Ordering::Acquire))
                .sum::<usize>(),
        ));
    });
    let (tasks, has_installed_marker, before_unlock, destroyed_before_unlock) = observations.unwrap();
    let statuses = tasks.map(|task| task.join());
    assert_eq!(before_unlock, [1, 1]);
    assert!(!has_installed_marker);
    assert_eq!(destroyed_before_unlock, 2);
    assert_eq!(statuses, [0, 0]);
    assert_eq!(results[0].load(Ordering::Acquire), 2);
    assert_eq!(results[1].load(Ordering::Acquire), 2);
    for count in destroyed {
        assert_eq!(Arc::strong_count(&count), 1);
    }
}

struct RepostGate {
    is_open: bool,
    in_flight: usize,
}

struct RepostControl {
    gate: kspin::SpinNoIrq<RepostGate>,
    reposted: AtomicUsize,
    destroyed: AtomicUsize,
    has_failed: AtomicBool,
    waker: Waker,
}

#[repr(C)]
struct RepostingProbe {
    node: CallbackNode,
    control: Arc<RepostControl>,
}

const _: () = core::assert!(core::mem::offset_of!(RepostingProbe, node) == 0);

impl Drop for RepostingProbe {
    fn drop(&mut self) {
        self.control.destroyed.fetch_add(1, Ordering::Release);
    }
}

/// # Safety
/// The node is the first field of the uniquely owned, unqueued probe Box.
unsafe fn cancel_reposting_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C), the offset assertion and unique ownership recover the Box.
    drop(unsafe { Box::from_raw(node.cast::<RepostingProbe>()) });
}

fn prepare_reposting_probe(probe: Box<RepostingProbe>) -> PreparedCallback {
    let record = Box::into_raw(probe);
    // SAFETY: project from the whole uniquely retained, unqueued Box.
    let node = NonNull::new(unsafe { &raw mut (*record).node }).unwrap();
    // SAFETY: stable unpublished Box transfers its unique submission right;
    // cancellation drops it without reposting and both paths are nonblocking.
    unsafe { PreparedCallback::new(node, cancel_reposting_probe) }
}

/// # Safety
/// The node projects the first field of a uniquely accepted probe Box.
/// The executor detached it and will not access it after this invocation.
unsafe fn repost_until_stopped(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the original Box base.
    let probe = unsafe { Box::from_raw(node.cast::<RepostingProbe>()) };
    let control = probe.control.clone();
    // SAFETY: no lock or retired data is held by this rejection probe.
    // Softirq must reject before installing a self-dependent drain.
    if unsafe { drain_callbacks() } != Err(KError::InvalidInput) {
        control.has_failed.store(true, Ordering::Release);
    }
    let can_repost = {
        let mut gate = control.gate.lock();
        if gate.is_open {
            gate.in_flight += 1;
            true
        } else {
            false
        }
    };
    if can_repost {
        let prepared = prepare_reposting_probe(probe);
        // A counted permit covers decision through publication. No gate lock
        // spans submit: no-IPI notification may wake the local daemon inline.
        // SAFETY: uniquely detached node, watched softirq CPU, no held locks;
        // its resources remain owned by the transferred Box and control Arc.
        match unsafe { prepared.submit() } {
            Ok(()) => {
                control.reposted.fetch_add(1, Ordering::Release);
            }
            Err((_, prepared)) => {
                drop(prepared);
                control.has_failed.store(true, Ordering::Release);
            }
        }
        control.gate.lock().in_flight -= 1;
    } else {
        drop(probe);
    }
    control.waker.wake_by_ref();
}

#[def_test(serial)]
fn stopped_reposts_drain_before_owner_teardown_and_reject_callback_waits() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let control = Arc::new(RepostControl {
        gate: kspin::SpinNoIrq::new(RepostGate {
            is_open: true,
            in_flight: 0,
        }),
        reposted: AtomicUsize::new(0),
        destroyed: AtomicUsize::new(0),
        has_failed: AtomicBool::new(false),
        waker: wait.waker(),
    });
    let prepared = prepare_reposting_probe(Box::new(RepostingProbe {
        node: CallbackNode::new(repost_until_stopped),
        control: control.clone(),
    }));
    // SAFETY: unpublished fixture with nonblocking callback/cancellation.
    // No fixture or scheduler lock spans public submission.
    let accepted = match unsafe { prepared.submit() } {
        Ok(()) => true,
        Err((_, prepared)) => {
            drop(prepared);
            false
        }
    };
    if accepted {
        while control.reposted.load(Ordering::Acquire) == 0
            && !control.has_failed.load(Ordering::Acquire)
        {
            wait.park();
        }
    }
    // Closing and granting permits use the same lock. Wait without the lock
    // until every earlier permit has finished submission; only then is stop
    // complete. A callback arriving later sees closed and cannot produce.
    control.gate.lock().is_open = false;
    loop {
        let has_producers = control.gate.lock().in_flight != 0;
        if !has_producers {
            break;
        }
        wait.park();
    }
    // SAFETY: gate closure plus zero in-flight permits proves all production
    // stopped. No callback-needed resource is held, and boot retains service.
    let drained = unsafe { drain_callbacks() };
    let reposted = control.reposted.load(Ordering::Acquire);
    let destroyed = control.destroyed.load(Ordering::Acquire);
    let has_failed = control.has_failed.load(Ordering::Acquire);
    let remaining_owners = Arc::strong_count(&control);
    let observer = Arc::downgrade(&control);
    drop(control);
    assert!(accepted);
    assert_eq!(drained, Ok(()));
    assert!(reposted > 0);
    assert_eq!(destroyed, 1);
    assert!(!has_failed);
    assert_eq!(remaining_owners, 1);
    assert!(observer.upgrade().is_none());
}
