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

/// Connection information for constructing outgoing vsock packet headers.
///
/// This is owned by the connection manager and mirrors the fields the
/// underlying virtio-vsock driver needs to fill packet headers. The transport
/// reads it but does not modify it (the manager updates it in place as credit
/// evolves).
#[derive(Copy, Clone, Debug, Default)]
pub struct VsockConnectionInfo {
    /// Connection identifier (peer address + local port).
    pub conn_id: VsockConnId,
    /// Bytes of receive buffer space we have advertised to the peer.
    pub buf_alloc: u32,
    /// Bytes of packet bodies we have forwarded to the application.
    pub fwd_cnt: u32,
    /// Last `buf_alloc` value advertised by the peer.
    pub peer_buf_alloc: u32,
    /// Last `fwd_cnt` value advertised by the peer.
    pub peer_fwd_cnt: u32,
    /// Cumulative bytes sent to the peer on this connection.
    pub tx_cnt: u32,
}

impl VsockConnectionInfo {
    /// Create a new info with the given connection ID and default credit.
    pub fn new(conn_id: VsockConnId) -> Self {
        Self {
            conn_id,
            buf_alloc: 0,
            fwd_cnt: 0,
            peer_buf_alloc: 0,
            peer_fwd_cnt: 0,
            tx_cnt: 0,
        }
    }

    /// Returns the available peer buffer space in bytes.
    pub fn peer_free(&self) -> u32 {
        self.peer_buf_alloc
            .wrapping_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
    }
}

/// Low-level, packet-oriented vsock device.
///
/// Implementors only need to know how to move virtio-vsock packets between the
/// guest and the host. Connection state, listen tables, accept queues, and
/// receive buffering live in the connection manager that consumes this trait.
pub trait VsockDevice: Device {
    /// Returns the CID which has been assigned to this guest.
    fn guest_cid(&self) -> u64;

    /// Register that the given local port is being listened on.
    fn listen(&self, port: u32);

    /// Unregister a previously listened port.
    fn unlisten(&self, port: u32);

    /// Send a connection request to the peer.
    fn connect(&self, info: &VsockConnectionInfo) -> DriverResult<()>;

    /// Accept an incoming connection request from the peer.
    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()>;

    /// Forcibly close or reject a connection by sending an RST packet.
    ///
    /// This is used both to reject an incoming connection request and to abort
    /// an already-established connection; the transport must not assume the
    /// connection exists in its own state.
    ///
    /// The full connection info is required because the underlying vsock header
    /// carries `buf_alloc` and `fwd_cnt` even in RST packets.
    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()>;

    /// Send a packet body to the peer.
    ///
    /// Returns the number of bytes sent, or [`DriverError::WouldBlock`] if the
    /// peer has no credit available.
    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize>;

    /// Request a clean shutdown of the connection.
    ///
    /// The full connection info is required because the underlying vsock header
    /// carries `buf_alloc` and `fwd_cnt` even in shutdown packets.
    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()>;

    /// Send a credit update to the peer.
    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()>;

    /// Poll the receive virtqueue for the next raw event.
    ///
    /// The handler is called once for each event, with the payload body for
    /// `Received` events. The transport releases its internal queue lock before
    /// invoking the handler, so the handler may safely call back into the
    /// transport to send control packets (accept, force_close, credit_update)
    /// without deadlocking.
    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool>;
}

/// A raw event delivered by the low-level vsock transport.
#[derive(Clone, Copy, Debug)]
pub struct VsockTransportEvent {
    /// The source of the event, i.e. the peer who sent it.
    pub source: VsockAddr,
    /// The destination of the event, i.e. the CID and port on our side.
    pub destination: VsockAddr,
    /// Peer credit from the packet header: the peer's advertised receive buffer
    /// space.
    pub peer_buf_alloc: u32,
    /// Peer credit from the packet header: the peer's forwarded byte count.
    pub peer_fwd_cnt: u32,
    /// The kind of event.
    pub kind: VsockTransportEventKind,
}

/// The kind of a raw vsock transport event.
#[derive(Clone, Copy, Debug)]
pub enum VsockTransportEventKind {
    /// The peer requests to establish a connection with us.
    ConnectionRequest,
    /// The connection was successfully established.
    Connected,
    /// Data was received on the connection.
    Received {
        /// The length of the data in bytes.
        length: usize,
    },
    /// The connection was closed.
    Disconnected,
    /// The peer sent us its credit information.
    CreditUpdate {
        /// The peer's total receive buffer space.
        buffer_allocation: u32,
        /// The peer's forwarded count.
        forward_count: u32,
    },
    /// The peer asked us to send our credit information.
    CreditRequest,
}
