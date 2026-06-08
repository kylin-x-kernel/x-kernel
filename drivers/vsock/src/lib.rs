// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for vsock device drivers.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

/// Vsock address.
#[derive(Copy, Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct VsockAddr {
    /// Context Identifier.
    pub cid: u64,
    /// Port number.
    pub port: u32,
}

/// Vsock connection id.
#[derive(Copy, Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct VsockConnId {
    /// Peer address.
    pub peer_addr: VsockAddr,
    /// Local port.
    pub local_port: u32,
}

impl VsockConnId {
    /// Create a new [`VsockConnId`] for listening socket
    pub fn listening(local_port: u32) -> Self {
        Self {
            peer_addr: VsockAddr { cid: 0, port: 0 },
            local_port,
        }
    }
}

/// Vsock driver event type.
#[derive(Debug)]
pub enum VsockDriverEventType {
    /// A connection request was received.
    ConnectionRequest(VsockConnId),
    /// A connection was established.
    Connected(VsockConnId),
    /// Data was received on a connection.
    Received(VsockConnId, usize),
    /// A connection was disconnected.
    Disconnected(VsockConnId),
    /// Credit Update
    CreditUpdate(VsockConnId),
    /// Unknown or unsupported event.
    Unknown,
}

/// Operations that require a vsock device driver to implement.
pub trait VsockDevice: Device {
    /// Returns the guest CID.
    fn guest_cid(&self) -> u64;

    /// Listen on a specific port.
    fn listen(&self, src_port: u32);

    /// Connect to a peer socket.
    fn connect(&self, cid: VsockConnId) -> DriverResult<()>;

    /// Send data to the connected peer socket.
    fn send(&self, cid: VsockConnId, buf: &[u8]) -> DriverResult<usize>;

    /// Receive data from the connected peer socket.
    fn recv(&self, cid: VsockConnId, buf: &mut [u8]) -> DriverResult<usize>;

    /// Returns the number of bytes in the receive buffer available to be read by recv.
    fn recv_avail(&self, cid: VsockConnId) -> DriverResult<usize>;

    /// Disconnect from the connected peer socket.
    fn disconnect(&self, cid: VsockConnId) -> DriverResult<()>;

    /// Forcibly closes the connection without waiting for the peer.
    fn abort(&self, cid: VsockConnId) -> DriverResult<()>;

    /// Poll for a device event.
    fn poll_event(&self) -> DriverResult<Option<VsockDriverEventType>>;
}
