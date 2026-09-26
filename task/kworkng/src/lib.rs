// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared ordinary and delayed work APIs with explicit ownership choices.
//!
//! # Choose an API
//!
//! | Use case | Preferred entry | Storage responsibility |
//! | --- | --- | --- |
//! | Ordinary task-context callback | [`WorkHandle`] | Shared safe ownership |
//! | Delayed task-context callback | [`DelayedWorkHandle`] | Owns Work and timer lifetime |
//! | Private normal queue with flush/close | [`NormalQueue`] | Queue lifecycle belongs to its users |
//! | Kernel-lifetime ordinary callback | [`queue_static_work`] | Requires `&'static Work` |
//! | Borrowed storage or non-sleeping BH callback | [`Work`], [`DelayedWork`], [`DefaultQueue`] | Unsafe publication; caller pins and drains |
//!
//! [`create_queue`] is the lower-level entry for an independently controlled
//! normal or BH queue. [`DefaultQueue`] grants submission authority only, not
//! whole-queue flush or shutdown. See `docs/design.md` and `docs/security.md` for
//! the service and lifetime model.
//!
//! # Context and readiness
//!
//! All submission examples below assume successful kernel NG initialization.
//! This crate does not activate services or replace legacy workqueues by itself.
//! Default selectors return [`ServiceError::NotReady`] before readiness; owned
//! handles and private queues reject construction before their providers exist.
//!
//! | Operation | Required context |
//! | --- | --- |
//! | Owned handle/queue preparation | Sleepable task; may allocate |
//! | Prepared queue/modify/cancel_pending | Task, BH or hard IRQ; no allocation or sleep |
//! | flush/cancel_sync/shutdown/close_and_drain | Sleepable task; allow callback progress |
//!
//! NMI submission is unsupported. Owned/static normal callbacks run in worker
//! tasks, never inline. BH callbacks must not sleep. For borrowed work, canceled
//! payload destruction must also obey the caller's context.
//!
//! `None` uses queue CPU policy or unfinished ownership; `Some(cpu)` selects an
//! allowed logical CPU but cannot override an unfinished Work owner. Default
//! queues use current-CPU policy. A pending occurrence rejects another ordinary
//! submission with [`SubmitError::AlreadyQueued`]; submission is not an event
//! counter. Handle rejection according to the application's event protocol, not
//! by retrying indefinitely in IRQ context. Example `unwrap` calls assume the
//! stated single-producer setup and are not an IRQ error-handling policy.
//!
//! # Ordinary owned work
//!
//! Prepare once in task context, then reuse the handle from producers, including
//! IRQ handlers. Clones share one Work identity and shutdown gate, not independent
//! submissions. Own captured state, for example with `Arc`; borrowed stack captures
//! are not accepted by the safe handle.
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::sync::Arc;
//! use core::sync::atomic::{AtomicUsize, Ordering};
//!
//! use kworkng::WorkHandle;
//!
//! let completed = Arc::new(AtomicUsize::new(0));
//! let state = completed.clone();
//! let work = WorkHandle::new(move || {
//!     state.fetch_add(1, Ordering::Relaxed);
//! })
//! .unwrap();
//! let producer = work.clone();
//! producer.queue_default(None).unwrap(); // This prepared call is IRQ-compatible.
//! work.flush().unwrap(); // Back in sleepable task context.
//! assert_eq!(completed.load(Ordering::Relaxed), 1);
//! drop(producer); // This example has no other producers.
//! work.shutdown().unwrap(); // Permanently closes all clones and drains.
//! ```
//!
//! [`WorkHandle::cancel_pending`] does not wait. [`WorkHandle::cancel_sync`] drains
//! captured work but allows later submissions; [`WorkHandle::flush`] waits without
//! canceling or closing. [`WorkHandle::shutdown`] permanently closes all clones and
//! drains. Last handle Drop is asynchronous reclamation, not a completion barrier,
//! and may cancel pending work. Use flush first when that callback must finish.
//! Shutdown drains execution but does not itself destroy captures held by handles.
//! Do not synchronously wait on this Work from its callback or hold resources it
//! needs; safe ownership prevents premature release, not dependency deadlocks.
//!
//! # Delayed owned work
//!
//! The handle prepares its timer once. Positive delays use monotonic time; zero
//! delay makes work ready without waiting for a tick and never runs it inline.
//! `queue_default` does not replace pending work; `modify_default` replaces its
//! delay or queues an occurrence when none is pending. A running callback is not
//! interrupted by modification.
//!
//! ```no_run
//! use ktime_types::TimeSpan;
//! use kworkng::DelayedWorkHandle;
//!
//! let work = DelayedWorkHandle::new(|| { /* task-context maintenance */ }).unwrap();
//! work.queue_default(TimeSpan::from_secs(1), None).unwrap();
//! work.modify_default(TimeSpan::from_secs(2), None).unwrap();
//! // Sleepable context, no concurrent producers: expedite and wait for this work.
//! work.flush().unwrap();
//! work.shutdown().unwrap(); // Closes admission and drains Work plus timer delivery.
//! ```
//!
//! Delayed flush expedites captured work; it is not a wait for the original timer
//! deadline and does not follow later retiming. Concurrent modification can retire
//! the captured obligation without executing it. To discard instead, use
//! [`DelayedWorkHandle::cancel_sync`] or permanent shutdown. Nonblocking cancellation
//! alone does not prove physical timer delivery has ended.
//!
//! # Private normal queue
//!
//! [`QueueConfig`] fixes allowed CPUs, routing and **per-CPU** active allowance,
//! not worker count. This example requires logical CPU 0 to be present. A single
//! allowed CPU with allowance 1 serializes that queue's released work; allowance 1
//! on several CPUs is not a global single-callback limit. Clones of [`NormalQueue`]
//! share closure state. Dropping a queue handle does not perform close-and-drain.
//!
//! ```no_run
//! use ktime_types::TimeSpan;
//! use kworkng::{CpuPolicy, DelayedWorkHandle, NormalQueue, QueueConfig, WorkHandle};
//!
//! let config = QueueConfig::new(&[0.into()], CpuPolicy::CurrentCpu, 1).unwrap();
//! let queue = NormalQueue::new("device-maintenance", config).unwrap();
//! let work = WorkHandle::new(|| {}).unwrap();
//! let delayed = DelayedWorkHandle::new(|| {}).unwrap();
//! work.queue(&queue, Some(0.into())).unwrap();
//! delayed
//!     .queue(&queue, TimeSpan::from_secs(1), Some(0.into()))
//!     .unwrap();
//! queue.flush().unwrap(); // Ordinary batch only, not timer-only arrangements.
//! // No external producers in this example. Real owners stop/join them first.
//! delayed.shutdown().unwrap(); // Drain each delayed handle BEFORE closing its queue.
//! work.shutdown().unwrap();
//! queue.close_and_drain().unwrap();
//! ```
//!
//! Private queue closure does not stop shared worker services or other queues.
//! For borrowed delayed work, stop/join producers and successfully call
//! [`DelayedWork::cancel_sync`] before closing the queue or freeing its storage.
//!
//! # Kernel-lifetime work
//!
//! `Work::with_callback` is not a const constructor. The following one-time
//! initialization deliberately retains a prepared allocation for kernel life;
//! it must not be repeated for each interrupt. Prefer [`WorkHandle`] when the
//! object must eventually be released.
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::boxed::Box;
//!
//! use kworkng::{Work, queue_static_work};
//!
//! fn service_device() {}
//! // One-time task-context preparation; retain this reference in the producer.
//! let work: &'static Work<fn()> =
//!     Box::leak(Box::new(Work::with_callback(service_device as fn())));
//! queue_static_work(work, None).unwrap(); // Safe, allocation-free publication.
//! work.flush().unwrap(); // Optional task-context wait, not closure.
//! ```
//!
//! # Borrowed work and BH callbacks
//!
//! Use this lower-level path only when the owner can guarantee pinning, retained
//! captures and final drain. `Pin<&Work>` alone does not keep storage alive. The
//! example has one producer and retains both stack objects through successful
//! cancellation. A real owner must stop/join all producers before the final drain.
//!
//! ```no_run
//! use core::{
//!     pin::pin,
//!     sync::atomic::{AtomicUsize, Ordering},
//! };
//!
//! use kworkng::{DefaultQueue, Work};
//!
//! let completed = AtomicUsize::new(0);
//! let work = pin!(Work::with_callback(|| {
//!     completed.fetch_add(1, Ordering::Relaxed);
//! }));
//! // SAFETY: sole producer; pinned Work and captures outlive successful drain.
//! // The callback only updates an atomic and never sleeps in BH context.
//! unsafe { DefaultQueue::BottomHalf.queue_work(work.as_ref(), None) }.unwrap();
//! // Sleepable owner context, without blocking callback progress.
//! work.cancel_sync().unwrap(); // Pending work may be canceled instead of executed.
//! ```
//!
//! Use [`DefaultQueue::Normal`] for a borrowed task-context callback or
//! [`DefaultQueue::HighBottomHalf`] for high-priority BH execution. BH is not a
//! sleepable worker. Borrowed delayed submission additionally retains the pinned
//! [`DelayedWork`] and timer until its own successful synchronous drain, even
//! after nonblocking cancellation reports no pending work.
//!
//! # Kernel startup integration
//!
//! Ordinary users do not initialize services. The kernel integrator calls this
//! once in sleepable context, after boot CPU mapping and task/timer/softirq
//! providers are ready on every present CPU, before publishing work producers.
//! Legacy Tasklet/High actions must not be installed alongside this provider.
//! CPU hotplug and public service teardown are unsupported.
//!
//! ```no_run
//! use core::num::NonZeroUsize;
//!
//! use ktime_types::TimeSpan;
//!
//! // Illustrative policy values, not fixed subsystem defaults.
//! kworkng::runtime::init(
//!     NonZeroUsize::new(4).unwrap(),  // Maximum normal workers per CPU.
//!     TimeSpan::from_secs(30),        // Idle worker retirement interval.
//!     NonZeroUsize::new(32).unwrap(), // BH source-inspection budget per pass.
//! )
//! .unwrap();
//! ```
//!
//! Default per-CPU queue allowances are configured separately by
//! `KWORKNG_NORMAL_MAX_ACTIVE` and `KWORKNG_BH_MAX_ACTIVE`.
//! `stall-check` enables warning-only backlog diagnostics; scheduling the watchdog
//! inspection is a separate integration step, with no GC or automatic recovery.
//! `self-wait-check` enables additional self-wait diagnostics, not permission for
//! callback dependency cycles. Neither feature changes storage ownership.

#![no_std]

extern crate alloc;

#[cfg(unittest)]
mod owned_tests;

mod defaults;
pub use defaults::{DefaultQueue, ServiceError};
mod queue;
pub use queue::{BottomHalfPriority, ExecutionKind, QueueCreateError, WorkQueue, create_queue};
mod normal_queue;
pub use normal_queue::{NormalQueue, queue_static_work};
mod owned;
pub use owned::HandleCreateError;
mod work_handle;
pub use work_handle::WorkHandle;
mod delayed_work_handle;
pub use delayed_work_handle::DelayedWorkHandle;

mod services;

/// Kernel startup integration, separate from ordinary queue-user APIs.
///
/// Call [`runtime::init`] once after all boot CPUs have task/timer/softirq providers and
/// before exposing work-producing drivers. This does not select NG automatically;
/// the startup integrator must exclude legacy BH action installation.
pub mod runtime {
    pub use kworkerpool_ng::{BottomHalfPrepareError, NormalRuntimeError};

    pub use crate::services::{InitError, init};
}

pub use kworkqueue_ng::{
    ArmError, CpuPolicy, DelayedFlushError, DelayedWork, DisableGuard, QueueConfig,
    QueueConfigError, SubmitError, WaitError, Work,
};
