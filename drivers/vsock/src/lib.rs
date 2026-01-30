//! Common traits and types for vsock device drivers.

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

#[doc(no_inline)]
pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};

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
    /// Unknown or unsupported event.
    Unknown,
}

/// Operations that require a vsock device driver to implement.
pub trait VsockDriverOps: DriverOps {
    /// Returns the guest CID.
    fn guest_cid(&self) -> u64;

    /// Listen on a specific port.
    fn listen(&mut self, src_port: u32);

    /// Connect to a peer socket.
    fn connect(&mut self, cid: VsockConnId) -> DriverResult<()>;

    /// Send data to the connected peer socket.
    fn send(&mut self, cid: VsockConnId, buf: &[u8]) -> DriverResult<usize>;

    /// Receive data from the connected peer socket.
    fn recv(&mut self, cid: VsockConnId, buf: &mut [u8]) -> DriverResult<usize>;

    /// Returns the number of bytes in the receive buffer available to be read by recv.
    fn recv_avail(&mut self, cid: VsockConnId) -> DriverResult<usize>;

    /// Disconnect from the connected peer socket.
    fn disconnect(&mut self, cid: VsockConnId) -> DriverResult<()>;

    /// Forcibly closes the connection without waiting for the peer.
    fn abort(&mut self, cid: VsockConnId) -> DriverResult<()>;

    /// Poll for a device event.
    fn poll_event(&mut self) -> DriverResult<Option<VsockDriverEventType>>;
}
