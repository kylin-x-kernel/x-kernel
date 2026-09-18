// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-local Trusty IPC handle ownership.
//!
//! Implement [`Handle`] for IPC objects, store them in [`HandleTable`], and
//! aggregate events with [`HandleSet`]. [`HandleWaitState`] provides waiter
//! notification and cookies. The process runtime owns table synchronization;
//! ports, channels, and user-memory access belong to callers.
//!
//! # Examples
//!
//! Install, look up, and close a handle. Examples require kernel allocation
//! and spin-lock support, so they are compile-only.
//!
//! ```no_run
//! use tipc_handle::{HandleKind, HandleSet, HandleTable};
//!
//! let mut table = HandleTable::new();
//! let set = HandleSet::handle_set_create();
//! let id = table.uctx_handle_install(set.clone())?;
//! assert_eq!(table.uctx_handle_get(id)?.kind(), HandleKind::HandleSet);
//! assert!(set.is_empty());
//! table.uctx_handle_remove(id)?;
//! assert!(table.uctx_handle_get(id).is_err());
//! # Ok::<(), kerrno::KError>(())
//! ```

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod handle;
mod handle_set;
mod handle_table;

/// Handle contracts, event masks, and shared waiter state.
/// Used by tipc objects to expose their kind, readiness, notifications, and cookies.
pub use handle::{
    Handle, HandleEventMask, HandleKind, HandleWaitState, IPC_HANDLE_POLL_ERROR,
    IPC_HANDLE_POLL_HUP, IPC_HANDLE_POLL_MSG, IPC_HANDLE_POLL_NONE, IPC_HANDLE_POLL_READY,
    IPC_HANDLE_POLL_SEND_UNBLOCKED,
};
/// Event aggregation, registration commands, and returned events.
/// Used to register multiple handles in a set and select events by their masks.
pub use handle_set::{
    HSET_ADD, HSET_DEL, HSET_DEL_GET_COOKIE, HSET_DEL_WITH_COOKIE, HSET_MOD, HSET_MOD_WITH_COOKIE,
    HandleSet, HandleSetCommand, HandleSetEntry, UEvent,
};
/// Process-local handle ownership and ID lookup.
/// Maps integer IDs to shared objects and manages removal, close, and wait snapshots.
pub use handle_table::HandleTable;
