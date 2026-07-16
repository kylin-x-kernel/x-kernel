// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-local Trusty IPC handle ownership.
//!
//! This crate owns the handle trait, event multiplexer, and process-local
//! integer handle table shared by the TIPC core and process runtime.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod handle;
mod handle_set;
mod handle_table;

pub use handle::{
    Handle, HandleEventMask, HandleKind, HandleWaitState, IPC_HANDLE_POLL_ERROR,
    IPC_HANDLE_POLL_HUP, IPC_HANDLE_POLL_MSG, IPC_HANDLE_POLL_NONE, IPC_HANDLE_POLL_READY,
    IPC_HANDLE_POLL_SEND_UNBLOCKED,
};
pub use handle_set::{
    HSET_ADD, HSET_DEL, HSET_DEL_GET_COOKIE, HSET_DEL_WITH_COOKIE, HSET_MOD, HSET_MOD_WITH_COOKIE,
    HandleSet, HandleSetCommand, HandleSetEntry, UEvent,
};
pub use handle_table::HandleTable;
