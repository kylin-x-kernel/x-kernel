// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The boot-owned service persists throughout these tests. Local counter
//! fixtures do not replace its driver, callbacks, or permanent marker storage.

use alloc::{boxed::Box, sync::Weak, task::Wake};
use core::sync::atomic::AtomicBool;

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::drain_callbacks;

#[def_test]
fn early_markers_cannot_finish_installation() {
    let wait = PreparedTaskWait::prepare().unwrap();
    let completion = DrainCompletion::new(wait.waker());
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    assert!(!completion.is_complete());
    // Install another marker only after the first has already finished.
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    assert!(!completion.is_complete());
    completion.finish_one();
    assert!(completion.is_complete());
}

#[def_test]
fn final_marker_after_installation_releases_wait() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let completion = DrainCompletion::new(wait.waker());
    completion.remaining.fetch_add(1, Ordering::Relaxed);
    completion.finish_one();
    wait.waker().wake();
    wait.park();
    assert!(!completion.is_complete());
    completion.finish_one();
    wait.park();
    assert!(completion.is_complete());
}

#[def_test]
fn drain_rejects_masked_and_read_scope_contexts() {
    let masked = {
        let _guard = kspin::IrqSave::new();
        // SAFETY: no callback-dependent resource is held; invalid wait context
        // must be rejected before acquiring the drain mutex or publishing nodes.
        unsafe { drain_callbacks() }
    };
    assert_eq!(masked, Err(KError::InvalidInput));
    let reader = ktask::current();
    let in_scope = crate::test_support::reader(&reader).with_read(|_| {
        // SAFETY: this fixture withdraws no object and only tests rejection of
        // an active read scope, before any drain can be installed.
        unsafe { drain_callbacks() }
    });
    assert_eq!(in_scope, Err(KError::InvalidInput));
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
/// The completed GP permits consuming that Box once outside queue locks.
unsafe fn destroy_probe(node: *mut CallbackNode) {
    // SAFETY: repr(C) and the offset assertion preserve the original Box base.
    drop(unsafe { Box::from_raw(node.cast::<DrainProbe>()) });
}

#[def_test(serial)]
fn drain_waits_for_every_cpu_and_reuses_markers_across_batches() {
    let service = SERVICE.get().unwrap();
    let destroyed = Arc::new(AtomicUsize::new(0));
    let per_cpu = super::super::CALLBACK_BUDGET * 2 + 1;
    let mut accepted = 0;
    for _ in 0..3 {
        for cpu in 0..kcpu_id_map::nr_cpus() {
            for _ in 0..per_cpu {
                let record = Box::into_raw(Box::new(DrainProbe {
                    node: CallbackNode::new(destroy_probe),
                    destroyed: destroyed.clone(),
                }));
                // SAFETY: project from the whole retained Box allocation.
                let node = unsafe { &raw mut (*record).node };
                // SAFETY: boot owns the participating queue and callback
                // carrier. The unpublished probe has no readers; its callback
                // consumes the unique Box nonblockingly after eligibility.
                let is_accepted = unsafe { service.callbacks.submit(cpu, node) };
                if !is_accepted {
                    // SAFETY: rejection left the Box caller-owned and untouched.
                    drop(unsafe { Box::from_raw(record) });
                }
                assert!(is_accepted);
                accepted += 1;
            }
        }
        // SAFETY: all producers in this fixture have stopped; no callback or
        // GP-needed resource is held. Boot retains every participating service.
        assert_eq!(unsafe { drain_callbacks() }, Ok(()));
        assert_eq!(destroyed.load(Ordering::Acquire), accepted);
        assert_eq!(Arc::strong_count(&destroyed), 1);
    }
    // Back-to-back drains must not reuse another pass's completion owner,
    // including when the prior marker's executor is still unwinding its batch.
    for _ in 0..3 {
        // SAFETY: same stopped producers and resource-free task context.
        assert_eq!(unsafe { drain_callbacks() }, Ok(()));
    }
}

#[def_test(serial)]
fn concurrent_drains_serialize_and_observe_completed_destruction() {
    let service = SERVICE.get().unwrap();
    let serial = service.drain.serialization.lock();
    let results = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let tasks: [_; 2] = core::array::from_fn(|id| {
        let results = results.clone();
        ktask::spawn(move || {
            let destroyed = Arc::new(AtomicUsize::new(0));
            let record = Box::into_raw(Box::new(DrainProbe {
                node: CallbackNode::new(destroy_probe),
                destroyed: destroyed.clone(),
            }));
            // SAFETY: retain the whole Box provenance in its node projection.
            let node = unsafe { &raw mut (*record).node };
            // SAFETY: uniquely owned unpublished fixture; destruction is
            // nonblocking, and the permanent service retains accepted storage.
            let accepted = unsafe { service.submit_callback(node) }.is_ok();
            if !accepted {
                // SAFETY: admission rejection leaves the Box caller-owned.
                drop(unsafe { Box::from_raw(record) });
            }
            // No other operation below can park before drain's mutex. Seeing
            // Blocked after this publication proves actual lock contention.
            results[id].store(1, Ordering::Release);
            // SAFETY: the producer has stopped; no callback-needed lock or
            // read scope is held by this task, and all CPU services stay live.
            let drained = unsafe { drain_callbacks() }.is_ok();
            let is_complete = accepted && drained
                && destroyed.load(Ordering::Acquire) == 1
                && Arc::strong_count(&destroyed) == 1;
            results[id].store(if is_complete { 2 } else { 3 }, Ordering::Release);
        })
    });
    for (id, task) in tasks.iter().enumerate() {
        while results[id].load(Ordering::Acquire) == 0
            || (results[id].load(Ordering::Acquire) == 1
                && task.state() != ktask::TaskState::Blocked)
        {
            // The outer bounded QEMU invocation diagnoses missing progress;
            // correctness depends on the observed blocked state, not duration.
            ktask::yield_now();
        }
    }
    let before_unlock = core::array::from_fn::<_, 2, _>(|id| results[id].load(Ordering::Acquire));
    let has_installed_marker = service.drain.markers.iter()
        .any(|marker| marker.completion.lock().is_some());
    drop(serial);
    let statuses = tasks.map(|task| task.join());
    assert_eq!(before_unlock, [1, 1]);
    assert!(!has_installed_marker);
    assert_eq!(statuses, [0, 0]);
    assert_eq!(results[0].load(Ordering::Acquire), 2);
    assert_eq!(results[1].load(Ordering::Acquire), 2);
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
    let node = unsafe { &raw mut (*record).node };
    // SAFETY: unpublished fixture with a preallocated task; callback only
    // activates it and never waits for the downstream gate held by this test.
    let accepted = unsafe { SERVICE.get().unwrap().submit_callback(node) }.is_ok();
    if !accepted {
        // SAFETY: rejection retained the original Box owner and its task ref.
        drop(unsafe { Box::from_raw(record) });
        // Ensure cleanup/join also completes on a rejected admission.
        ktask::activate_task(&worker);
    }
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

struct ReuseMarkerWake {
    marker: &'static DrainMarker,
    previous: Weak<DrainCompletion>,
    next: Arc<DrainCompletion>,
    was_previous_retained: Arc<AtomicBool>,
}

impl Wake for ReuseMarkerWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.was_previous_retained.store(self.previous.upgrade().is_some(), Ordering::Release);
        // Model the next installer running before the old wakeup unwinds.
        // This also detects a marker lock accidentally held across wakeup.
        *self.marker.completion.lock() = Some(self.next.clone());
    }
}

#[def_test(serial)]
fn marker_reuse_during_old_wakeup_keeps_completion_owners_separate() {
    let service = SERVICE.get().unwrap();
    // SAFETY: stop prior fixtures' callbacks before manipulating an unqueued
    // permanent marker; no resource required by those callbacks is held.
    let drained = unsafe { drain_callbacks() };
    assert_eq!(drained, Ok(()));
    let _serial = service.drain.serialization.lock();
    let marker = &service.drain.markers[0];
    let next = Arc::new(DrainCompletion::new(Waker::noop().clone()));
    let was_retained = Arc::new(AtomicBool::new(false));
    let previous = Arc::new_cyclic(|previous| DrainCompletion::new(Waker::from(Arc::new(
        ReuseMarkerWake {
            marker,
            previous: previous.clone(),
            next: next.clone(),
            was_previous_retained: was_retained.clone(),
        },
    ))));
    let observer = Arc::downgrade(&previous);
    *marker.completion.lock() = Some(previous);
    let record = core::ptr::from_ref(marker);
    // SAFETY: project from the whole permanently retained marker allocation.
    let node = unsafe { &raw const (*record).node }.cast_mut();
    // SAFETY: exclusive installation under the drain mutex; the prior drain
    // removed all marker membership. This unqueued completion-only fixture
    // directly invokes once without asking the service executor to run it.
    unsafe { complete_marker(node) };
    let installed = marker.completion.lock().take();
    assert!(was_retained.load(Ordering::Acquire));
    assert!(observer.upgrade().is_none());
    assert!(installed.as_ref().is_some_and(|installed| Arc::ptr_eq(installed, &next)));
    assert!(!next.is_complete());
    assert_eq!(next.remaining.load(Ordering::Relaxed), 1);
}

struct RepostControl {
    can_repost: SpinNoIrq<bool>,
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
/// `node` projects the first field of a uniquely accepted RepostingProbe Box.
/// The executor has detached it, holds no queue lock and never accesses it again.
unsafe fn repost_until_stopped(node: *mut CallbackNode) {
    // SAFETY: the offset assertion and raw projection preserve the Box base.
    let probe = unsafe { Box::from_raw(node.cast::<RepostingProbe>()) };
    let control = probe.control.clone();
    // SAFETY: this rejection probe holds no lock and withdraws no data.
    // Softirq context must reject before installing a self-dependent wait.
    if unsafe { drain_callbacks() } != Err(KError::InvalidInput) {
        control.has_failed.store(true, Ordering::Release);
    }
    let can_repost = control.can_repost.lock();
    let mut was_accepted = false;
    if *can_repost {
        let record = Box::into_raw(probe);
        // SAFETY: project the embedded node from the complete original Box.
        let node = unsafe { &raw mut (*record).node };
        let cpu = khal::percpu::this_cpu_id().as_usize();
        // SAFETY: the short producer lock pins this watched softirq CPU and
        // serializes the last repost against shutdown. This detached node has
        // unique ownership; repost requires a new GP. Only queue bookkeeping
        // runs under the lock, never notification or callback execution.
        was_accepted = unsafe { SERVICE.get().unwrap().callbacks.submit(cpu, node) };
        if was_accepted {
            control.reposted.fetch_add(1, Ordering::Release);
        } else {
            // SAFETY: rejection retained this exact Box allocation.
            drop(unsafe { Box::from_raw(record) });
            control.has_failed.store(true, Ordering::Release);
        }
    } else {
        drop(probe);
    }
    drop(can_repost);
    // The node may already be queued again. Only the independent control Arc
    // is used below; notification cannot nest the producer and scheduler locks.
    if was_accepted {
        SERVICE.get().unwrap().request_pass();
    }
    control.waker.wake_by_ref();
}

#[def_test(serial)]
fn stopped_reposts_drain_before_owner_teardown_and_reject_callback_waits() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let control = Arc::new(RepostControl {
        can_repost: SpinNoIrq::new(true),
        reposted: AtomicUsize::new(0),
        destroyed: AtomicUsize::new(0),
        has_failed: AtomicBool::new(false),
        waker: wait.waker(),
    });
    let record = Box::into_raw(Box::new(RepostingProbe {
        node: CallbackNode::new(repost_until_stopped),
        control: control.clone(),
    }));
    // SAFETY: raw projection retains the whole uniquely owned allocation.
    let node = unsafe { &raw mut (*record).node };
    // SAFETY: the unpublished probe has no protected readers. Its callback
    // either transfers the same node once or destroys it without blocking.
    let accepted = unsafe { SERVICE.get().unwrap().submit_callback(node) }.is_ok();
    if !accepted {
        // SAFETY: rejection retains the Box and has installed no callback.
        drop(unsafe { Box::from_raw(record) });
    } else {
        while control.reposted.load(Ordering::Acquire) == 0
            && !control.has_failed.load(Ordering::Acquire)
        {
            wait.park();
        }
    }
    // This lock covers the callback's decision AND publication. After release
    // every permitted repost is already visible; none can arrive after drain.
    *control.can_repost.lock() = false;
    // SAFETY: all external production has stopped and the repost gate is now
    // closed. No producer lock or resource needed by callbacks is held.
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
