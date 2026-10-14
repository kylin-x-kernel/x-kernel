// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-device I/O reclaiming: pending marking, softirq processing and stop.
//!
//! Virtio activation flow:
//! - Probe creates one `Arc<BlockIoReclaimer>` and keeps the driver device alive.
//!   It shares the same reclaimer with IRQ dispatch once at setup.
//! - IRQ dispatch calls `reclaimer.mark_pending()` after device ack.
//! - Block softirq calls the driver's `process_completed_requests()` operation.
//! - Close finishes admitted I/O, synchronizes IRQs, then calls `stop_and_wait()`.
//!
//! One `BlockIoReclaimer` stores host processing state for one device,
//! not one request. Request identities, results and buffers belong to the driver;
//! caller waiting resources are implemented separately in `block_completion`.

use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;

use block::completion::{BlockCompletionOperations, BlockSignals, BlockWaiter};
use kirq::softirq::{self, SoftirqVec};
use kspin::{NoPreempt, SpinNoIrq, static_lock};
use ksync::Mutex;
use linked_list_r4l::{GetLinks, Links, List};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessingPhase {
    Idle,
    Queued(usize),
    Running(usize),
    RunningAgain(usize),
}

struct ProcessingState {
    phase: ProcessingPhase,
    is_accepting: bool,
    stop_notification: Option<Arc<dyn BlockSignals>>,
}

/// Host-owned completion-processing state for one block device.
///
/// Holds a weak device reference, pending-list membership and processing phase.
/// It contains no request buffers, request results or hardware descriptors.
/// IRQ dispatch and task-context shutdown operate on this same object.
/// Repeated processing requests share this allocation. Dropping an Arc does not
/// stop processing: activation must explicitly stop before destroying the device.
pub(crate) struct BlockIoReclaimer {
    // Intrusive membership avoids allocating a node in the IRQ path.
    pending_link: Links<Self>,
    // Activation owns the device; an escaped IRQ handle must not keep it alive.
    device: Weak<dyn BlockCompletionOperations>,
    // The pending-list lock also protects this phase and the closing-task signal.
    state: UnsafeCell<ProcessingState>,
    // Only closing tasks take this lock, held through the terminal wait so a
    // second closer cannot overwrite the first closer's stop_notification.
    stop_lock: Mutex<()>,
}

// SAFETY: PENDING_DEVICES protects state and all mutations of pending_link.
// Arc-owned reclaimers never move while linked. The device is
// Send + Sync and no driver callback executes under this lock.
unsafe impl Sync for BlockIoReclaimer {}

impl GetLinks for BlockIoReclaimer {
    type EntryType = Self;

    fn get_links(data: &Self) -> &Links<Self> {
        &data.pending_link
    }
}

static_lock! {
    static PENDING_DEVICES: SpinNoIrq<[List<Arc<BlockIoReclaimer>>; kbuild_config::NR_CPUS]> =
        SpinNoIrq::new([const { List::new() }; kbuild_config::NR_CPUS]);
}

fn update_state<R>(
    reclaimer: &BlockIoReclaimer,
    update: impl FnOnce(&mut ProcessingState, &mut [List<Arc<BlockIoReclaimer>>]) -> R,
) -> R {
    let mut pending = PENDING_DEVICES.lock();
    // SAFETY: this is the actual global lock shared with batch consumption.
    // The borrowed reclaimer stays alive; the closure cannot return references to
    // either mutable argument. No callback or notification runs under this lock.
    update(unsafe { &mut *reclaimer.state.get() }, &mut *pending)
}

/// Installs the Block softirq handler before devices can request completion processing.
pub(crate) fn init() {
    assert!(softirq::open_softirq(
        SoftirqVec::Block,
        process_pending_devices
    ));
}

#[cfg_attr(all(not(unittest), not(feature = "virtio-blk")), expect(dead_code))]
impl BlockIoReclaimer {
    /// Creates stable per-device state in task context before IRQ publication.
    pub(crate) fn new(device: Weak<dyn BlockCompletionOperations>) -> Arc<Self> {
        Arc::new(Self {
            pending_link: Links::new(),
            device,
            state: UnsafeCell::new(ProcessingState {
                phase: ProcessingPhase::Idle,
                is_accepting: true,
                stop_notification: None,
            }),
            stop_lock: Mutex::new(()),
        })
    }

    /// Marks this device as needing completion processing by the Block softirq.
    ///
    /// Call after acknowledging a device IRQ. If idle, queues the device and
    /// raises the softirq; if queued, coalesces the notification; if running,
    /// requests another pass. Calls after stopping are ignored.
    ///
    /// Does not register an IRQ, submit I/O, reclaim requests inline or wait.
    /// Non-sleeping in hardirq, softirq and task contexts. Pending-list operations
    /// do not allocate; existing host scheduler wake costs are unchanged.
    pub(crate) fn mark_pending(self: &Arc<Self>) {
        let _pin = NoPreempt::new();
        let cpu = khal::percpu::this_cpu_id().as_usize();
        let should_raise = update_state(self, |state, pending| {
            if !state.is_accepting {
                return false;
            }
            match state.phase {
                ProcessingPhase::Idle => {
                    state.phase = ProcessingPhase::Queued(cpu);
                    pending[cpu].push_back(self.clone());
                    true
                }
                ProcessingPhase::Running(owner) => {
                    state.phase = ProcessingPhase::RunningAgain(owner);
                    false
                }
                ProcessingPhase::Queued(_) | ProcessingPhase::RunningAgain(_) => false,
            }
        });
        // Daemon wake can enter the scheduler: release PENDING_DEVICES first, but
        // stay pinned until the enqueue's owning CPU has its pending bit.
        if should_raise {
            softirq::raise_softirq(SoftirqVec::Block);
        }
    }

    /// Rejects processing requests and waits for queued/running callbacks to retire.
    ///
    /// This stops completion processing, not in-flight I/O. Close must drain
    /// admitted I/O and synchronize the device IRQ producer before this call.
    ///
    /// The caller supplies a fresh private terminal waiter prepared on this
    /// closing task before teardown. No locks may forbid sleeping; the device
    /// must not call this on its own callback. Online CPUs must keep servicing
    /// their softirqs. Concurrent callers serialize on a task-only mutex;
    /// repeated calls with fresh waiters complete once the first stop retires.
    pub(crate) fn stop_and_wait(&self, waiter: &mut dyn BlockWaiter) {
        let _stop = self.stop_lock.lock();
        let signals = waiter.signals();
        let is_idle = update_state(self, |state, _| {
            state.is_accepting = false;
            if state.phase == ProcessingPhase::Idle {
                true
            } else {
                state.stop_notification = Some(signals.clone());
                false
            }
        });
        if is_idle {
            signals.notify_completion();
        }
        waiter.wait_completion();
    }
}

fn process_pending_devices() {
    let _pin = NoPreempt::new();
    let cpu = khal::percpu::this_cpu_id().as_usize();
    let mut batch = {
        let mut pending = PENDING_DEVICES.lock();
        core::mem::take(&mut pending[cpu])
    };
    loop {
        let (reclaimer, should_run) = {
            let _pending = PENDING_DEVICES.lock();
            let Some(reclaimer) = batch.pop_front() else {
                break;
            };
            // SAFETY: the popped Arc owns the reclaimer and PENDING_DEVICES serializes
            // state access against remote processing requests and stop. Detached entries
            // remain Queued(cpu) until this transition; they cannot relink.
            let state = unsafe { &mut *reclaimer.state.get() };
            assert_eq!(state.phase, ProcessingPhase::Queued(cpu));
            state.phase = ProcessingPhase::Running(cpu);
            let should_run = state.is_accepting;
            (reclaimer, should_run)
        };
        if should_run && let Some(device) = reclaimer.device.upgrade() {
            device.process_completed_requests();
        }
        // The callback Arc has dropped before Idle or stop can be published.
        let (should_raise, stop_notification) = update_state(&reclaimer, |state, pending| {
            if state.is_accepting && state.phase == ProcessingPhase::RunningAgain(cpu) {
                state.phase = ProcessingPhase::Queued(cpu);
                pending[cpu].push_back(reclaimer.clone());
                (true, None)
            } else {
                state.phase = ProcessingPhase::Idle;
                (false, state.stop_notification.take())
            }
        });
        if should_raise {
            softirq::raise_softirq(SoftirqVec::Block);
        }
        if let Some(signals) = stop_notification {
            signals.notify_completion();
        }
    }
}

unittest::path_mod_test!(
    block_completion_dispatch_unittest,
    "../ut-tests/block_completion_dispatch_unittest.rs"
);
