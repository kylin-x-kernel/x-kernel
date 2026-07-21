// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trusty IPC core.
//!
//! This crate owns the transport-independent TIPC state machines and Trusty
//! syscall ABI adapter, while process-local handle ownership lives in
//! `tipc-handle`.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod channel;
pub mod error;
mod memref;
mod message;
mod port;
mod registry;
pub mod syscall;

use bitflags::bitflags;
pub use channel::{IpcChan, IpcChanState};
pub use memref::{
    MMAP_FLAG_PROT_EXEC, MMAP_FLAG_PROT_MASK, MMAP_FLAG_PROT_MTE, MMAP_FLAG_PROT_READ,
    MMAP_FLAG_PROT_WRITE, MemRef,
};
pub use message::IpcMsgInfo;
pub use port::{IpcPort, IpcPortFlags, IpcPortState};
pub use registry::{ipc_port_connect_async, ipc_port_create, ipc_port_publish};
pub use tipc_handle::{
    HSET_ADD, HSET_DEL, HSET_DEL_GET_COOKIE, HSET_DEL_WITH_COOKIE, HSET_MOD, HSET_MOD_WITH_COOKIE,
    Handle, HandleEventMask, HandleKind, HandleSet, HandleSetCommand, HandleSetEntry, HandleTable,
    HandleWaitState, IPC_HANDLE_POLL_ERROR, IPC_HANDLE_POLL_HUP, IPC_HANDLE_POLL_MSG,
    IPC_HANDLE_POLL_NONE, IPC_HANDLE_POLL_READY, IPC_HANDLE_POLL_SEND_UNBLOCKED, UEvent,
};

/// Maximum byte length of a TIPC service path, excluding a trailing NUL.
pub const IPC_PORT_PATH_MAX: usize = 64;
/// Maximum number of receive buffers in one channel queue.
pub const IPC_CHAN_MAX_BUFS: usize = 32;
/// Maximum size of one receive buffer.
pub const IPC_CHAN_MAX_BUF_SIZE: usize = 4096;
/// Maximum number of handles attached to one message.
pub const IPC_MAX_MSG_HANDLES: usize = 8;
/// Marks the server endpoint of a channel pair.
pub const IPC_CHAN_FLAG_SERVER: u32 = 0x1;
/// The local endpoint may retry a previously blocked send.
pub const IPC_CHAN_AUX_STATE_SEND_UNBLOCKED: u32 = 1 << 2;
/// The server accepted this channel.
pub const IPC_CHAN_AUX_STATE_CONNECTED: u32 = 1 << 3;

/// Wait until a port with the requested path is published.
pub const IPC_CONNECT_WAIT_FOR_PORT: u32 = 0x1;
/// Return before the server accepts the connection.
pub const IPC_CONNECT_ASYNC: u32 = 0x2;

/// Stable identity supplied by the caller when opening a TIPC connection.
///
/// TIPC intentionally treats this as opaque data so that TEE and non-TEE
/// clients can provide identities without coupling the IPC core to either.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub struct IpcUuid(uuid::Uuid);

impl IpcUuid {
    /// Creates a UUID from its canonical byte representation.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(uuid::Uuid::from_bytes(bytes))
    }

    /// Returns the canonical byte representation.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    /// Creates an identity from the crate-internal UUID representation.
    pub(crate) const fn from_uuid(uuid: uuid::Uuid) -> Self {
        Self(uuid)
    }

    /// Returns the crate-internal UUID representation.
    pub(crate) const fn into_uuid(self) -> uuid::Uuid {
        self.0
    }
}

bitflags! {
    /// Options accepted by [`ipc_port_connect_async`].
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct IpcConnectFlags: u32 {
        /// Wait until a service with the requested path is published.
        const WAIT_FOR_PORT = IPC_CONNECT_WAIT_FOR_PORT;
        /// Return the client endpoint before the server accepts it.
        const ASYNC = IPC_CONNECT_ASYNC;
    }
}

#[cfg(unittest)]
mod tests;
