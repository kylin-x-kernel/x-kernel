// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel RCU state, execution-context integration and progress service.
//!
//! The algorithm lives in `krcu-core`; this crate owns its system instances,
//! task/timer/softirq execution and deferred-cleanup policy. Scheduler and
//! interrupt entry points notify it through the statically linked integration
//! contract. [`Rcu`] provides owning task-context publication and replacement;
//! [`PreparedCallback`] transfers intrusive callback ownership, and
//! [`drain_callbacks`] waits for prior callbacks. [`with_read`] supplies a
//! borrowed [`ReadScope`] for multi-object reads; [`synchronize_rcu`] supports
//! audited low-level lifetime protocols. Core reader accounting stays internal.
//!
//! # Mechanism
//!
//! RCU separates publishing a replacement from reclaiming the old allocation.
//! A reader borrows the published value inside a read scope. An updater first
//! withdraws the old value, then waits for a subsequent grace period before
//! reclaiming it. Readers that could still hold that value delay reclamation;
//! later readers do not extend that same obligation indefinitely. Several old
//! versions may coexist while their respective readers finish.
//!
//! A grace period does not lock the data, serialize writers, or make multiple
//! publications one atomic snapshot. Payload initialization must precede
//! publication; interior mutation still needs its own synchronization.
//!
//! # Operations And Ownership
//!
//! [`Rcu::with_read`] and [`Rcu::replace_sync`] provide the first two operations.
//! [`PreparedCallback::submit`] and [`drain_callbacks`] provide the asynchronous
//! lifetime and teardown operations:
//!
//! - Scoped reading borrows a value without acquiring ownership of it. Task
//!   readers may be preempted or migrated but may not voluntarily sleep/yield.
//!   Initialized hardirq, softirq and NMI handlers use [`with_interrupt_read`]
//!   and must finish the scope before handler return without scheduling.
//! - Owning synchronous replacement prepares its wait before publication,
//!   exchanges the allocation, and returns the old owner after its grace
//!   period. Preparation failure leaves publication unchanged and returns the
//!   unused replacement. Destruction then runs in the caller's task context.
//! - Prepared callback submission transfers an existing stable node without
//!   allocating a wrapper. Rejection returns its ownership. Acceptance permits
//!   invocation even before submission returns; the callback executes later
//!   than its required grace period, in non-sleepable softirq context.
//! - Callback draining waits for previously queued callbacks to return. It is
//!   not a substitute for withdrawing published data and waiting for readers.
//!   Teardown must stop producers/reposts first, and separately join any work
//!   launched by a callback.
//!
//! Synchronous waits and drains require sleepable task context outside read
//! scopes. Callback submission rejects NMI; NMI read participation does not
//! imply permission to submit, allocate, wait, or reclaim there.
//! [`preflight_callback_submission`] checks task context and permanent service
//! readiness before a destructive update; it transfers no ownership and does
//! not replace the submission contract or certify reclamation.
//!
//! # Interrupt Handler Example
//!
//! Prepare and publish the container in task context. The handler only borrows
//! it after the normal interrupt entry has established current and watching;
//! this API does not support early exception-entry assembly windows.
//!
//! ```no_run
//! fn handle_device_irq(configuration: &krcu::Rcu<u64>) -> u64 {
//!     krcu::with_interrupt_read(|scope| *configuration.read(scope))
//! }
//! ```
//!
//! # Owning Publication Example
//!
//! Run this in sleepable kernel task context after service initialization.
//! Readers copy a result out of the scope; replacement returns the old owner
//! only when it is safe to destroy. Sharing the container between tasks can use
//! `Arc<Rcu<T>>`; this does not add a reference count to each protected read.
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::{boxed::Box, string::String};
//!
//! use krcu::Rcu;
//!
//! let name = Rcu::new(Box::new(String::from("old")));
//! let snapshot = name.with_read(|current| current.clone());
//! match name.replace_sync(Box::new(String::from("new"))) {
//!     Ok(old) => drop(old),
//!     Err((_error, unused)) => drop(unused), // Publication is unchanged.
//! }
//! assert_eq!(snapshot, "old");
//! ```
//!
//! # One Scope, Several Objects
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::boxed::Box;
//!
//! use krcu::{Rcu, with_read};
//!
//! let left = Rcu::new(Box::new(11));
//! let right = Rcu::new(Box::new(22));
//! let total = with_read(|scope| *left.read(scope) + *right.read(scope));
//! assert_eq!(total, 33);
//! ```
//!
//! This protects both lifetimes, not consistency between two independently
//! published versions. Use one container for a snapshot that must be replaced
//! atomically. [`Rcu::replace_sync`] already waits; adding a separate
//! [`synchronize_rcu`] afterward is unnecessary. The standalone operation is
//! for unsafe integrations that withdraw and retain their own objects: on an
//! error they must keep those objects alive, not treat failure as permission
//! to reclaim. It waits for readers, unlike [`drain_callbacks`].
//!
//! # Intrusive Callback Example
//!
//! A callback node can live inside existing storage; preparing the handle and
//! submitting it add no allocation. This example uses a fresh, unpublished
//! record so cancellation can immediately free it. A real retired object must
//! retain protection on rejection: do not copy this cancellation policy for
//! data still accessible to readers. This is a low-level callback example,
//! not an asynchronous replacement operation on [`Rcu`].
//!
//! ```no_run
//! extern crate alloc;
//! use alloc::boxed::Box;
//! use core::ptr::NonNull;
//!
//! use krcu::{CallbackNode, PreparedCallback, drain_callbacks};
//!
//! #[repr(C)]
//! struct Record {
//!     node: CallbackNode,
//!     value: u64,
//! }
//! const _: () = assert!(core::mem::offset_of!(Record, node) == 0);
//!
//! /// # Safety
//! /// Consume exactly the raw Box owner created below, through its zero-offset
//! /// node projection. There must be no remaining readers or queue accesses.
//! unsafe fn release_record(node: *mut CallbackNode) {
//!     // SAFETY: repr(C), the offset assertion and the original field projection
//!     // preserve this allocation's base and provenance. Drop cannot block.
//!     drop(unsafe { Box::from_raw(node.cast::<Record>()) });
//! }
//!
//! fn submit_and_drain() -> kerrno::KResult<()> {
//!     let record = Box::into_raw(Box::new(Record {
//!         node: CallbackNode::new(release_record),
//!         value: 42,
//!     }));
//!     // SAFETY: raw projection retains the whole Box allocation's provenance.
//!     let node = unsafe { &raw mut (*record).node };
//!     // SAFETY: a live Box field cannot be null.
//!     let node = unsafe { NonNull::new_unchecked(node) };
//!     // SAFETY: unique stable storage, transferable nonblocking cleanup, never
//!     // published to readers. Cancellation and invocation each consume one owner.
//!     let prepared = unsafe { PreparedCallback::new(node, release_record) };
//!     // SAFETY: no publication routes or locks; callback code is permanent.
//!     if let Err((error, returned)) = unsafe { prepared.submit() } {
//!         drop(returned); // Safe here only because this record was unpublished.
//!         return Err(error);
//!     }
//!     // Do not access record: the callback may already have freed it.
//!     // SAFETY: sleepable task context, no resource needed by callbacks held,
//!     // and no further submissions/reposts. Kernel services remain available.
//!     unsafe { drain_callbacks() }
//! }
//! ```
//!
//! # Integration Boundary
//!
//! [`init_service`] is a boot integration operation. The user-boundary callbacks
//! are architecture-transfer hooks, not read-side primitives. Standalone
//! `krcu-core` reader tokens do not prove participation in this kernel service.

#![no_std]

extern crate alloc;

mod context;
#[doc(hidden)]
pub mod host;
mod read;
mod service;

pub use context::{enter_user_quiescent, exit_user_quiescent};
pub use krcu_core::CallbackNode;
pub use read::{ReadScope, with_interrupt_read, with_read};
#[cfg(unittest)]
#[doc(hidden)]
pub use service::has_installed_drain_markers_for_tests;
pub use service::{
    PreparedCallback, Rcu, drain_callbacks, init_service, preflight_callback_submission,
    synchronize_rcu,
};

/// Observes deferred-cleanup demand without consuming it or authorizing cleanup.
///
/// Kernel integration fixtures must pass a discovered CPU with published slots.
/// This snapshot neither pins the caller nor reports a quiescent state.
#[cfg(unittest)]
#[doc(hidden)]
pub fn reader_cleanup_requested_for_tests(cpu: kcpu_id_map::LogicalCpuId) -> bool {
    context::needs_reader_cleanup(cpu)
}
