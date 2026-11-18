//! Driver for VirtIO socket devices.

use super::DEFAULT_RX_BUFFER_SIZE;
use super::error::SocketError;
use super::protocol::{
    Feature, StreamShutdown, VirtioVsockConfig, VirtioVsockHdr, VirtioVsockOp, VsockAddr,
};
use crate::config::read_config;
use crate::hal::Hal;
use crate::queue::{OwningQueue, SyncError, VirtQueue};
use crate::transport::Transport;
use crate::{Error, Result};
use core::mem::size_of;
use log::{debug, error};
use zerocopy::{FromBytes, IntoBytes};

/// Runs `$expr`; on failure stops the device before returning the error.
///
/// The result is bound before the match so the mutable borrow taken by `$expr`
/// ends before `reset` is called.
macro_rules! try_or_reset {
    ($transport:ident, $expr:expr) => {{
        let result = $expr;
        match result {
            Ok(value) => value,
            Err(error) => {
                $transport.reset();
                return Err(error);
            }
        }
    }};
}

pub(crate) const RX_QUEUE_IDX: u16 = 0;
pub(crate) const TX_QUEUE_IDX: u16 = 1;
const EVENT_QUEUE_IDX: u16 = 2;

/// Virtqueue depth used for RX, TX, and event queues.
///
/// Matches the 128-entry depth exposed by QEMU's vhost-vsock device and used
/// by the Linux guest driver.
pub const QUEUE_SIZE: usize = 128;
/// Size in bytes of the virtio-vsock header shared by RX and TX packets.
pub const VSOCK_HEADER_SIZE: usize = size_of::<VirtioVsockHdr>();

const SUPPORTED_FEATURES: Feature = Feature::RING_EVENT_IDX
    .union(Feature::RING_INDIRECT_DESC)
    .union(Feature::VERSION_1)
    .union(Feature::ACCESS_PLATFORM);

/// The virtio-vsock operation encoded in an outgoing packet.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VsockTxPacketKind {
    /// A data packet carrying `payload_len` body bytes.
    Data {
        /// Number of payload bytes following the header.
        payload_len: u32,
    },
    /// A connection request.
    Request,
    /// A connection response.
    Response,
    /// A connection reset or rejection.
    Reset,
    /// A shutdown request carrying the given stream hints.
    Shutdown(StreamShutdown),
    /// An updated advertised receive window.
    CreditUpdate,
    /// A request for the peer's current credit.
    CreditRequest,
}

/// Information about a particular vsock connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// The address of the peer.
    pub dst: VsockAddr,
    /// The local port number associated with the connection.
    pub src_port: u32,
    /// The last `buf_alloc` value the peer sent to us, indicating how much receive buffer space in
    /// bytes it has allocated for packet bodies.
    peer_buf_alloc: u32,
    /// The last `fwd_cnt` value the peer sent to us, indicating how many bytes of packet bodies it
    /// has finished processing.
    peer_fwd_cnt: u32,
    /// The number of bytes of packet bodies which we have sent to the peer.
    tx_cnt: u32,
    /// The number of bytes of buffer space we have allocated to receive packet bodies from the
    /// peer.
    pub buf_alloc: u32,
    /// The number of bytes of packet bodies which we have received from the peer and handled.
    fwd_cnt: u32,
    /// Whether we have recently requested credit from the peer.
    ///
    /// This is set to true when we send a `VIRTIO_VSOCK_OP_CREDIT_REQUEST`, and false when we
    /// receive a `VIRTIO_VSOCK_OP_CREDIT_UPDATE`.
    has_pending_credit_request: bool,
}

impl ConnectionInfo {
    /// Creates a new `ConnectionInfo` for the given peer address and local port, and default values
    /// for everything else.
    pub fn new(destination: VsockAddr, src_port: u32) -> Self {
        Self {
            dst: destination,
            src_port,
            ..Default::default()
        }
    }

    /// Updates this connection info with the peer buffer allocation and forwarded count from the
    /// given event.
    pub fn update_for_event(&mut self, event: &VsockEvent) {
        self.peer_buf_alloc = event.buffer_status.buffer_allocation;
        self.peer_fwd_cnt = event.buffer_status.forward_count;

        if let VsockEventType::CreditUpdate = event.event_type {
            self.has_pending_credit_request = false;
        }
    }

    /// Increases the forwarded count recorded for this connection by the given number of bytes.
    ///
    /// This should be called once received data has been passed to the client, so there is buffer
    /// space available for more.
    pub fn done_forwarding(&mut self, length: usize) {
        self.fwd_cnt = self.fwd_cnt.wrapping_add(length as u32);
    }

    /// Sets the number of bytes of packet bodies which we have sent to the peer.
    pub fn set_tx_cnt(&mut self, tx_cnt: u32) {
        self.tx_cnt = tx_cnt;
    }

    /// Sets the number of bytes of packet bodies which we have received from the
    /// peer and handled.
    pub fn set_fwd_cnt(&mut self, fwd_cnt: u32) {
        self.fwd_cnt = fwd_cnt;
    }

    /// Sets the last `buf_alloc` value the peer sent to us.
    pub fn set_peer_buf_alloc(&mut self, peer_buf_alloc: u32) {
        self.peer_buf_alloc = peer_buf_alloc;
    }

    /// Sets the last `fwd_cnt` value the peer sent to us.
    pub fn set_peer_fwd_cnt(&mut self, peer_fwd_cnt: u32) {
        self.peer_fwd_cnt = peer_fwd_cnt;
    }

    /// Returns the number of bytes of packet bodies which we have sent to the peer.
    pub fn tx_cnt(&self) -> u32 {
        self.tx_cnt
    }

    /// Returns the number of bytes of packet bodies which we have received from
    /// the peer and handled.
    pub fn fwd_cnt(&self) -> u32 {
        self.fwd_cnt
    }

    /// Returns the number of bytes of RX buffer space the peer has available to receive packet body
    /// data from us.
    fn peer_free(&self) -> u32 {
        let outstanding = self.tx_cnt.wrapping_sub(self.peer_fwd_cnt);
        // If the peer sent inconsistent vsock packet headers and `outstanding` is larger than
        // `peer_buf_alloc` `wrapping_sub` could produce an incorrect and large value. There's no
        // way to recover from a buggy peer at this level so we use saturating_sub to map
        // `outstanding > peer_buf_alloc` to zero.
        self.peer_buf_alloc.saturating_sub(outstanding)
    }

    fn new_header(&self, src_cid: u64) -> VirtioVsockHdr {
        VirtioVsockHdr {
            src_cid: src_cid.into(),
            dst_cid: self.dst.cid.into(),
            src_port: self.src_port.into(),
            dst_port: self.dst.port.into(),
            buf_alloc: self.buf_alloc.into(),
            fwd_cnt: self.fwd_cnt.into(),
            ..Default::default()
        }
    }
}

/// An event received from a VirtIO socket device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsockEvent {
    /// The source of the event, i.e. the peer who sent it.
    pub source: VsockAddr,
    /// The destination of the event, i.e. the CID and port on our side.
    pub destination: VsockAddr,
    /// The peer's buffer status for the connection.
    pub buffer_status: VsockBufferStatus,
    /// The type of event.
    pub event_type: VsockEventType,
}

impl VsockEvent {
    /// Returns whether the event matches the given connection.
    pub fn matches_connection(&self, connection_info: &ConnectionInfo, guest_cid: u64) -> bool {
        self.source == connection_info.dst
            && self.destination.cid == guest_cid
            && self.destination.port == connection_info.src_port
    }

    fn from_header(header: &VirtioVsockHdr) -> Result<Self> {
        let op = header.op()?;
        let buffer_status = VsockBufferStatus {
            buffer_allocation: header.buf_alloc.into(),
            forward_count: header.fwd_cnt.into(),
        };
        let source = header.source();
        let destination = header.destination();

        let event_type = match op {
            VirtioVsockOp::Request => {
                header.check_data_is_empty()?;
                VsockEventType::ConnectionRequest
            }
            VirtioVsockOp::Response => {
                header.check_data_is_empty()?;
                VsockEventType::Connected
            }
            VirtioVsockOp::CreditUpdate => {
                header.check_data_is_empty()?;
                VsockEventType::CreditUpdate
            }
            VirtioVsockOp::Rst | VirtioVsockOp::Shutdown => {
                header.check_data_is_empty()?;
                debug!("Disconnected from the peer");
                let reason = if op == VirtioVsockOp::Rst {
                    DisconnectReason::Reset
                } else {
                    DisconnectReason::Shutdown
                };
                VsockEventType::Disconnected { reason }
            }
            VirtioVsockOp::Rw => VsockEventType::Received {
                length: header.len() as usize,
            },
            VirtioVsockOp::CreditRequest => {
                header.check_data_is_empty()?;
                VsockEventType::CreditRequest
            }
            VirtioVsockOp::Invalid => return Err(SocketError::InvalidOperation.into()),
        };

        Ok(VsockEvent {
            source,
            destination,
            buffer_status,
            event_type,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VsockBufferStatus {
    pub buffer_allocation: u32,
    pub forward_count: u32,
}

/// The reason why a vsock connection was closed.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DisconnectReason {
    /// The peer has either closed the connection in response to our shutdown request, or forcibly
    /// closed it of its own accord.
    Reset,
    /// The peer asked to shut down the connection.
    Shutdown,
}

/// Details of the type of an event received from a VirtIO socket.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VsockEventType {
    /// The peer requests to establish a connection with us.
    ConnectionRequest,
    /// The connection was successfully established.
    Connected,
    /// The connection was closed.
    Disconnected {
        /// The reason for the disconnection.
        reason: DisconnectReason,
    },
    /// Data was received on the connection.
    Received {
        /// The length of the data in bytes.
        length: usize,
    },
    /// The peer requests us to send a credit update.
    CreditRequest,
    /// The peer just sent us a credit update with nothing else.
    CreditUpdate,
}

/// Low-level driver for a VirtIO socket device.
///
/// You probably want to use [`VsockConnectionManager`](super::VsockConnectionManager) rather than
/// using this directly.
///
/// `RX_BUFFER_SIZE` is the size in bytes of each buffer used in the RX virtqueue. This must be
/// bigger than `size_of::<VirtioVsockHdr>()`.
pub struct VirtIOSocket<H: Hal, T: Transport, const RX_BUFFER_SIZE: usize = DEFAULT_RX_BUFFER_SIZE>
{
    /// Virtqueue to receive packets. Drops (detach/unshare/free) before the
    /// rings and transport.
    rx: OwningQueue<H, QUEUE_SIZE, RX_BUFFER_SIZE>,
    tx: VirtQueue<H, { QUEUE_SIZE }>,
    /// Virtqueue to receive events from the device.
    event: VirtQueue<H, { QUEUE_SIZE }>,
    /// The guest_cid field contains the guest’s context ID, which uniquely identifies
    /// the device for its lifetime. The upper 32 bits of the CID are reserved and zeroed.
    guest_cid: u64,
    /// Set once the device has been stopped after a fatal TX failure. No queue
    /// operation is allowed after that.
    failed: bool,
    /// Declared last so the queues and their DMA mappings are released before
    /// the transport which the device can still access.
    transport: T,
}

impl<H: Hal, T: Transport, const RX_BUFFER_SIZE: usize> Drop
    for VirtIOSocket<H, T, RX_BUFFER_SIZE>
{
    fn drop(&mut self) {
        // Clear any pointers pointing to DMA regions, so the device doesn't try to access them
        // after they have been freed.
        self.transport.queue_unset(RX_QUEUE_IDX);
        self.transport.queue_unset(TX_QUEUE_IDX);
        self.transport.queue_unset(EVENT_QUEUE_IDX);
        // Device-side stop before the field destructors release queue memory.
        self.transport.reset();
    }
}

impl<H: Hal, T: Transport, const RX_BUFFER_SIZE: usize> VirtIOSocket<H, T, RX_BUFFER_SIZE> {
    /// Create a new VirtIO Vsock driver.
    pub fn new(mut transport: T) -> Result<Self> {
        assert!(RX_BUFFER_SIZE > size_of::<VirtioVsockHdr>());

        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        let guest_cid = try_or_reset!(
            transport,
            transport.read_consistent(|| {
                Ok(
                    (read_config!(transport, VirtioVsockConfig, guest_cid_low)? as u64)
                        | ((read_config!(transport, VirtioVsockConfig, guest_cid_high)? as u64)
                            << 32),
                )
            })
        );
        debug!("guest cid: {guest_cid:?}");

        let rx = try_or_reset!(
            transport,
            VirtQueue::new(
                &mut transport,
                RX_QUEUE_IDX,
                negotiated_features.contains(Feature::RING_INDIRECT_DESC),
                negotiated_features.contains(Feature::RING_EVENT_IDX),
                negotiated_features.contains(Feature::ACCESS_PLATFORM),
            )
        );
        let tx = try_or_reset!(
            transport,
            VirtQueue::new(
                &mut transport,
                TX_QUEUE_IDX,
                negotiated_features.contains(Feature::RING_INDIRECT_DESC),
                negotiated_features.contains(Feature::RING_EVENT_IDX),
                negotiated_features.contains(Feature::ACCESS_PLATFORM),
            )
        );
        let event = try_or_reset!(
            transport,
            VirtQueue::new(
                &mut transport,
                EVENT_QUEUE_IDX,
                negotiated_features.contains(Feature::RING_INDIRECT_DESC),
                negotiated_features.contains(Feature::RING_EVENT_IDX),
                negotiated_features.contains(Feature::ACCESS_PLATFORM),
            )
        );

        let mut rx = OwningQueue::empty(rx);
        // SAFETY: This constructor is the device owner. If filling fails or any
        // later step fails, it resets before returning; its local queues are
        // dropped after that reset and the transport parameter is dropped last.
        if let Err(error) = unsafe { rx.fill() } {
            transport.reset();
            return Err(error);
        }

        // No fallible operation is allowed after buffers are published, so the
        // reset obligation above covers the only failure path.
        transport.finish_init();
        if rx.should_notify() {
            transport.notify(RX_QUEUE_IDX);
        }

        Ok(Self {
            rx,
            tx,
            event,
            guest_cid,
            failed: false,
            transport,
        })
    }

    /// Returns the CID which has been assigned to this guest.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    /// Encodes the virtio-vsock header for an outgoing packet.
    ///
    /// `packet` must contain exactly the header followed by the payload bytes described by `kind`.
    /// This helper keeps the wire layout in the device crate while allowing callers to own packet
    /// storage for asynchronous transmission.
    ///
    /// # Errors
    ///
    /// Returns [`SocketError::BufferTooShort`] or [`SocketError::BufferTooLong`] when `packet`
    /// does not have exactly the length required by `kind`.
    pub fn prepare_tx_packet(
        &self,
        kind: VsockTxPacketKind,
        connection_info: &ConnectionInfo,
        packet: &mut [u8],
    ) -> Result {
        let (op, payload_len, flags, expected_len) = match kind {
            VsockTxPacketKind::Data { payload_len } => (
                VirtioVsockOp::Rw,
                payload_len,
                0,
                VSOCK_HEADER_SIZE
                    .checked_add(payload_len as usize)
                    .ok_or(SocketError::InvalidNumber)?,
            ),
            VsockTxPacketKind::Request => {
                (VirtioVsockOp::Request, 0, 0, VSOCK_HEADER_SIZE)
            }
            VsockTxPacketKind::Response => {
                (VirtioVsockOp::Response, 0, 0, VSOCK_HEADER_SIZE)
            }
            VsockTxPacketKind::Reset => {
                (VirtioVsockOp::Rst, 0, 0, VSOCK_HEADER_SIZE)
            }
            VsockTxPacketKind::Shutdown(hints) => {
                (VirtioVsockOp::Shutdown, 0, hints.bits(), VSOCK_HEADER_SIZE)
            }
            VsockTxPacketKind::CreditUpdate => {
                (VirtioVsockOp::CreditUpdate, 0, 0, VSOCK_HEADER_SIZE)
            }
            VsockTxPacketKind::CreditRequest => {
                (VirtioVsockOp::CreditRequest, 0, 0, VSOCK_HEADER_SIZE)
            }
        };

        if packet.len() < expected_len {
            return Err(SocketError::BufferTooShort.into());
        }
        if packet.len() > expected_len {
            return Err(SocketError::BufferTooLong(packet.len(), expected_len).into());
        }

        let header = VirtioVsockHdr {
            op: op.into(),
            len: payload_len.into(),
            flags: flags.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        packet[..VSOCK_HEADER_SIZE].copy_from_slice(header.as_bytes());
        Ok(())
    }

    /// Returns the number of available TX descriptors.
    pub fn available_tx_descriptors(&self) -> usize {
        self.tx.available_desc()
    }

    /// Submits a fully encoded packet to the TX virtqueue and notifies the device.
    ///
    /// This returns without waiting for completion. The returned token must eventually be passed
    /// to [`Self::transmit_complete`] with the exact same packet.
    ///
    /// # Safety
    ///
    /// `packet` must remain valid and must not be accessed until the matching
    /// [`Self::transmit_complete`] call succeeds.
    pub unsafe fn transmit_begin(&mut self, packet: &[u8]) -> Result<u16> {
        if packet.len() < VSOCK_HEADER_SIZE {
            return Err(SocketError::BufferTooShort.into());
        }
        // SAFETY: The caller guarantees that `packet` remains valid until completion.
        let token = unsafe { self.tx.add(&[packet], &mut []) }?;
        if self.tx.should_notify() {
            self.transport.notify(TX_QUEUE_IDX);
        }
        Ok(token)
    }

    /// Returns the token of the next completed TX descriptor, without consuming it.
    pub fn poll_transmit(&self) -> Option<u16> {
        self.tx.peek_used()
    }

    /// Reclaims a completed TX descriptor.
    ///
    /// # Safety
    ///
    /// `packet` must be the exact buffer that was submitted with `token` by
    /// [`Self::transmit_begin`], and `token` must be the next completed descriptor returned by
    /// [`Self::poll_transmit`].
    pub unsafe fn transmit_complete(&mut self, token: u16, packet: &[u8]) -> Result {
        // SAFETY: The caller guarantees that `packet` matches the descriptor identified by
        // `token`, and that `token` is the next completed entry.
        unsafe { self.tx.pop_used(token, &[packet], &mut [])? };
        Ok(())
    }

    /// Sends a request to connect to the given destination.
    ///
    /// This returns as soon as the request is sent; you should wait until `poll` returns a
    /// `VsockEventType::Connected` event indicating that the peer has accepted the connection
    /// before sending data.
    pub fn connect(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Request.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        // Sends a header only packet to the TX queue to connect the device to the listening socket
        // at the given destination.
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Accepts the given connection from a peer.
    pub fn accept(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Response.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Requests the peer to send us a credit update for the given connection.
    fn request_credit(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::CreditRequest.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Sends the buffer to the destination.
    pub fn send(&mut self, buffer: &[u8], connection_info: &mut ConnectionInfo) -> Result {
        self.check_peer_buffer_is_sufficient(connection_info, buffer.len())?;

        let len = buffer.len() as u32;
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Rw.into(),
            len: len.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        connection_info.tx_cnt = connection_info.tx_cnt.wrapping_add(len);
        self.send_packet_to_tx_queue(&header, buffer)
    }

    fn check_peer_buffer_is_sufficient(
        &mut self,
        connection_info: &mut ConnectionInfo,
        buffer_len: usize,
    ) -> Result {
        if connection_info.peer_free() as usize >= buffer_len {
            Ok(())
        } else {
            // Request an update of the cached peer credit, if we haven't already done so, and tell
            // the caller to try again later.
            if !connection_info.has_pending_credit_request {
                self.request_credit(connection_info)?;
                connection_info.has_pending_credit_request = true;
            }
            Err(SocketError::InsufficientBufferSpaceInPeer.into())
        }
    }

    /// Tells the peer how much buffer space we have to receive data.
    pub fn credit_update(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::CreditUpdate.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Polls the RX virtqueue for the next event, and calls the given handler function to handle
    /// it.
    pub fn poll(
        &mut self,
        handler: impl FnOnce(VsockEvent, &[u8]) -> Result<Option<VsockEvent>>,
    ) -> Result<Option<VsockEvent>> {
        if self.failed {
            return Err(Error::IoError);
        }
        self.rx.poll(&mut self.transport, |buffer| {
            let (header, body) = read_header_and_body(buffer)?;
            VsockEvent::from_header(&header).and_then(|event| handler(event, body))
        })
    }

    /// Requests to shut down the connection cleanly, sending hints about whether we will send or
    /// receive more data.
    ///
    /// This returns as soon as the request is sent; you should wait until `poll` returns a
    /// `VsockEventType::Disconnected` event if you want to know that the peer has acknowledged the
    /// shutdown.
    pub fn shutdown_with_hints(
        &mut self,
        connection_info: &ConnectionInfo,
        hints: StreamShutdown,
    ) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Shutdown.into(),
            flags: hints.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])
    }

    /// Requests to shut down the connection cleanly, telling the peer that we won't send or receive
    /// any more data.
    ///
    /// This returns as soon as the request is sent; you should wait until `poll` returns a
    /// `VsockEventType::Disconnected` event if you want to know that the peer has acknowledged the
    /// shutdown.
    pub fn shutdown(&mut self, connection_info: &ConnectionInfo) -> Result {
        self.shutdown_with_hints(
            connection_info,
            StreamShutdown::SEND | StreamShutdown::RECEIVE,
        )
    }

    /// Forcibly closes the connection without waiting for the peer.
    pub fn force_close(&mut self, connection_info: &ConnectionInfo) -> Result {
        let header = VirtioVsockHdr {
            op: VirtioVsockOp::Rst.into(),
            ..connection_info.new_header(self.guest_cid)
        };
        self.send_packet_to_tx_queue(&header, &[])?;
        Ok(())
    }

    fn send_packet_to_tx_queue(&mut self, header: &VirtioVsockHdr, buffer: &[u8]) -> Result {
        if self.failed {
            return Err(Error::IoError);
        }

        // Both arrays are bound here so `inputs` outlives the checked call and
        // any detach which follows it in this frame.
        let header_only = [header.as_bytes()];
        let header_and_body = [header.as_bytes(), buffer];
        let inputs: &[&[u8]] = if buffer.is_empty() {
            &header_only
        } else {
            &header_and_body
        };

        // SAFETY: `inputs` borrows `header` (a caller-owned stack value) and
        // `buffer` (the caller's slice), both of which stay valid for this
        // frame. If publication already happened when this returns an error, we
        // stop the device and detach the token below before the borrows end.
        let result = unsafe {
            self.tx
                .add_notify_wait_pop_checked(inputs, &mut [], &mut self.transport)
        };
        match result {
            Ok(_) => Ok(()),
            Err(SyncError {
                error,
                token: None,
            }) => Err(error),
            Err(SyncError {
                error,
                token: Some(token),
            }) => {
                // Stop the device before touching the published request. The
                // transport reset is the device-side evidence that it no longer
                // accesses the old queues and buffers.
                self.transport.reset();
                // SAFETY: The device has been reset. `inputs` is the same
                // buffer set passed to the failing `add` and is still valid.
                unsafe {
                    self.tx.detach_unused_buffer(token, inputs, &mut []);
                }
                // SAFETY: The device has been reset, and this socket will not
                // use these queues again. Releasing RX here is device-level
                // cleanup, not a queue-local rollback.
                unsafe {
                    self.rx.detach_all_after_stop();
                }
                self.failed = true;
                error!("vsock TX failed after publication, device stopped: {error:?}");
                Err(Error::IoError)
            }
        }
    }
}

fn read_header_and_body(buffer: &[u8]) -> Result<(VirtioVsockHdr, &[u8])> {
    // This could fail if the device returns a buffer used length shorter than the header size.
    let header = VirtioVsockHdr::read_from_prefix(buffer)
        .map_err(|_| SocketError::BufferTooShort)?
        .0;
    let body_length = header.len() as usize;

    // This could fail if the device returns an unreasonably long body length.
    let data_end = VSOCK_HEADER_SIZE
        .checked_add(body_length)
        .ok_or(SocketError::InvalidNumber)?;
    // This could fail if the device returns a body length longer than buffer used length it
    // returned.
    let data = buffer
        .get(VSOCK_HEADER_SIZE..data_end)
        .ok_or(SocketError::BufferTooShort)?;
    Ok((header, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BufferDirection, PhysAddr,
        config::ReadOnly,
        hal::fake::FakeHal,
        queue::fake_corrupt_last_used_id,
        transport::{
            DeviceStatus, DeviceType, InterruptStatus,
            fake::{FakeTransport, QueueStatus, State},
        },
    };
    use alloc::{sync::Arc, vec, vec::Vec};
    use core::{cell::RefCell, ptr::NonNull};
    use std::sync::Mutex;
    use zerocopy::Immutable;

    /// DMA calls observed through [`CountingHal`].
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    struct Counts {
        alloc: usize,
        dealloc: usize,
        share: usize,
        unshare: usize,
        fail_alloc_at: Option<usize>,
        fail_share_at: Option<usize>,
    }

    /// Ordered evidence shared by the counting HAL and the recording transport.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Event {
        Alloc,
        Dealloc,
        Share,
        Unshare,
        Notify(u16),
        QueueUnset(u16),
        Reset,
        TransportDrop,
    }

    thread_local! {
        static COUNTS: RefCell<Counts> = RefCell::new(Counts::default());
        static EVENTS: RefCell<Vec<Event>> = RefCell::new(Vec::new());
    }

    fn trace(event: Event) {
        EVENTS.with(|events| events.borrow_mut().push(event));
    }

    fn events() -> Vec<Event> {
        EVENTS.with(|events| events.borrow().clone())
    }

    fn counts() -> Counts {
        COUNTS.with(|counts| *counts.borrow())
    }

    fn reset_harness() {
        COUNTS.with(|counts| *counts.borrow_mut() = Counts::default());
        EVENTS.with(|events| events.borrow_mut().clear());
    }

    /// Makes the given one-based `share` call fail.
    fn fail_share_at(call: usize) {
        COUNTS.with(|counts| counts.borrow_mut().fail_share_at = Some(call));
    }

    /// Makes the given one-based `dma_alloc` call fail.
    fn fail_alloc_at(call: usize) {
        COUNTS.with(|counts| counts.borrow_mut().fail_alloc_at = Some(call));
    }

    fn clear_failures() {
        COUNTS.with(|counts| {
            let mut counts = counts.borrow_mut();
            counts.fail_alloc_at = None;
            counts.fail_share_at = None;
        });
    }

    fn position(events: &[Event], event: Event) -> usize {
        events
            .iter()
            .position(|candidate| *candidate == event)
            .unwrap_or_else(|| panic!("{event:?} missing from {events:?}"))
    }

    fn last_position(events: &[Event], event: Event) -> usize {
        events
            .iter()
            .rposition(|candidate| *candidate == event)
            .unwrap_or_else(|| panic!("{event:?} missing from {events:?}"))
    }

    fn count_events(events: &[Event], event: Event) -> usize {
        events.iter().filter(|candidate| **candidate == event).count()
    }

    /// HAL which counts and orders DMA operations and can inject failures.
    ///
    /// Every successful operation is forwarded to [`FakeHal`], so the queue,
    /// buffer and mapping objects under test are the real ones.
    struct CountingHal;

    // SAFETY: every operation forwards to `FakeHal`, which upholds the `Hal`
    // contract. Counting only observes arguments; injected failures return the
    // documented failure sentinel and release nothing.
    unsafe impl Hal for CountingHal {
        fn dma_alloc(
            pages: usize,
            direction: BufferDirection,
            access_platform: bool,
        ) -> (PhysAddr, NonNull<u8>) {
            let fail = COUNTS.with(|counts| {
                let mut counts = counts.borrow_mut();
                counts.alloc += 1;
                counts.fail_alloc_at == Some(counts.alloc)
            });
            trace(Event::Alloc);
            if fail {
                (0, NonNull::dangling())
            } else {
                <FakeHal as Hal>::dma_alloc(pages, direction, access_platform)
            }
        }

        unsafe fn dma_dealloc(
            paddr: PhysAddr,
            vaddr: NonNull<u8>,
            pages: usize,
            access_platform: bool,
        ) -> i32 {
            COUNTS.with(|counts| counts.borrow_mut().dealloc += 1);
            trace(Event::Dealloc);
            // SAFETY: forwarded unchanged; the caller upholds the allocation contract.
            unsafe { <FakeHal as Hal>::dma_dealloc(paddr, vaddr, pages, access_platform) }
        }

        unsafe fn mmio_phys_to_virt(paddr: PhysAddr, size: usize) -> NonNull<u8> {
            // SAFETY: forwarded unchanged; the caller supplies a valid MMIO range.
            unsafe { <FakeHal as Hal>::mmio_phys_to_virt(paddr, size) }
        }

        unsafe fn share(
            buffer: NonNull<[u8]>,
            direction: BufferDirection,
            access_platform: bool,
        ) -> PhysAddr {
            let fail = COUNTS.with(|counts| {
                let mut counts = counts.borrow_mut();
                counts.share += 1;
                counts.fail_share_at == Some(counts.share)
            });
            trace(Event::Share);
            if fail {
                0
            } else {
                // SAFETY: forwarded unchanged; the caller keeps the buffer valid.
                unsafe { <FakeHal as Hal>::share(buffer, direction, access_platform) }
            }
        }

        unsafe fn unshare(
            paddr: PhysAddr,
            buffer: NonNull<[u8]>,
            direction: BufferDirection,
            access_platform: bool,
        ) {
            COUNTS.with(|counts| counts.borrow_mut().unshare += 1);
            trace(Event::Unshare);
            // SAFETY: forwarded unchanged; the caller stopped device access and
            // keeps the buffer valid.
            unsafe { <FakeHal as Hal>::unshare(paddr, buffer, direction, access_platform) }
        }
    }

    /// Transport wrapper which records lifecycle operations and can complete TX.
    struct RecordingTransport {
        inner: FakeTransport<VirtioVsockConfig>,
        complete_tx: bool,
        corrupt_tx_id: Option<u16>,
    }

    impl RecordingTransport {
        fn new(device_features: u64) -> (Self, Arc<Mutex<State<VirtioVsockConfig>>>) {
            let state = Arc::new(Mutex::new(State::new(
                vec![
                    QueueStatus::default(),
                    QueueStatus::default(),
                    QueueStatus::default(),
                ],
                VirtioVsockConfig {
                    guest_cid_low: ReadOnly::new(66),
                    guest_cid_high: ReadOnly::new(0),
                },
            )));
            (
                Self {
                    inner: FakeTransport {
                        device_type: DeviceType::Socket,
                        max_queue_size: QUEUE_SIZE as u32,
                        device_features,
                        state: state.clone(),
                    },
                    complete_tx: false,
                    corrupt_tx_id: None,
                },
                state,
            )
        }
    }

    impl Drop for RecordingTransport {
        fn drop(&mut self) {
            trace(Event::TransportDrop);
        }
    }

    impl Transport for RecordingTransport {
        fn device_type(&self) -> DeviceType {
            self.inner.device_type()
        }

        fn read_device_features(&mut self) -> u64 {
            self.inner.read_device_features()
        }

        fn write_driver_features(&mut self, features: u64) {
            self.inner.write_driver_features(features);
        }

        fn max_queue_size(&mut self, queue: u16) -> u32 {
            self.inner.max_queue_size(queue)
        }

        fn notify(&mut self, queue: u16) {
            trace(Event::Notify(queue));
            self.inner.notify(queue);
            if queue == TX_QUEUE_IDX && (self.complete_tx || self.corrupt_tx_id.is_some()) {
                let state = self.inner.state.clone();
                let mut state = state.lock().unwrap();
                state.read_from_queue::<QUEUE_SIZE>(TX_QUEUE_IDX);
                if let Some(id) = self.corrupt_tx_id {
                    let queue = &state.queues[usize::from(TX_QUEUE_IDX)];
                    fake_corrupt_last_used_id::<QUEUE_SIZE>(queue.device_area as *mut u8, id);
                }
            }
        }

        fn get_status(&self) -> DeviceStatus {
            self.inner.get_status()
        }

        fn set_status(&mut self, status: DeviceStatus) {
            self.inner.set_status(status);
        }

        fn set_guest_page_size(&mut self, size: u32) {
            self.inner.set_guest_page_size(size);
        }

        fn requires_legacy_layout(&self) -> bool {
            self.inner.requires_legacy_layout()
        }

        fn queue_set(
            &mut self,
            queue: u16,
            size: u32,
            descriptors: PhysAddr,
            driver_area: PhysAddr,
            device_area: PhysAddr,
        ) {
            self.inner
                .queue_set(queue, size, descriptors, driver_area, device_area);
        }

        fn queue_unset(&mut self, queue: u16) {
            trace(Event::QueueUnset(queue));
            self.inner.queue_unset(queue);
        }

        fn queue_used(&mut self, queue: u16) -> bool {
            self.inner.queue_used(queue)
        }

        fn ack_interrupt(&mut self) -> InterruptStatus {
            self.inner.ack_interrupt()
        }

        fn read_config_generation(&self) -> u32 {
            self.inner.read_config_generation()
        }

        fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T> {
            self.inner.read_config_space(offset)
        }

        fn write_config_space<T: IntoBytes + Immutable>(
            &mut self,
            offset: usize,
            value: T,
        ) -> Result<()> {
            self.inner.write_config_space(offset, value)
        }

        fn reset(&mut self) {
            trace(Event::Reset);
            Transport::reset(&mut self.inner);
        }
    }

    fn connection() -> ConnectionInfo {
        let mut connection = ConnectionInfo::new(VsockAddr { cid: 3, port: 1024 }, 42);
        connection.set_peer_buf_alloc(4096);
        connection
    }

    fn test_socket() -> (
        VirtIOSocket<FakeHal, FakeTransport<VirtioVsockConfig>>,
        Arc<Mutex<State<VirtioVsockConfig>>>,
    ) {
        let config_space = VirtioVsockConfig {
            guest_cid_low: ReadOnly::new(66),
            guest_cid_high: ReadOnly::new(0),
        };
        let state = Arc::new(Mutex::new(State::new(
            vec![
                QueueStatus::default(),
                QueueStatus::default(),
                QueueStatus::default(),
            ],
            config_space,
        )));
        let transport = FakeTransport {
            device_type: DeviceType::Socket,
            max_queue_size: QUEUE_SIZE as u32,
            device_features: 0,
            state: state.clone(),
        };
        let socket =
            VirtIOSocket::<FakeHal, FakeTransport<VirtioVsockConfig>>::new(transport).unwrap();
        (socket, state)
    }

    fn test_connection(peer_buf_alloc: u32) -> ConnectionInfo {
        let mut connection = ConnectionInfo::new(VsockAddr { cid: 2, port: 1234 }, 4321);
        connection.buf_alloc = 1024;
        connection.set_peer_buf_alloc(peer_buf_alloc);
        connection
    }

    #[test]
    fn config() {
        let (socket, _) = test_socket();
        assert_eq!(socket.guest_cid(), 0x00_0000_0042);
    }

    #[test]
    fn drop_stops_device_before_releasing_unconsumed_rx_buffers() {
        reset_harness();
        let (transport, state) = RecordingTransport::new(0);
        let socket = VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).unwrap();
        assert_eq!(counts().share, QUEUE_SIZE);
        assert_eq!(counts().alloc, 6);
        assert_eq!(counts().unshare, 0);

        drop(socket);

        let counts = counts();
        assert_eq!(counts.share, QUEUE_SIZE);
        assert_eq!(counts.unshare, QUEUE_SIZE);
        assert_eq!(counts.alloc, 6);
        assert_eq!(counts.dealloc, 6);

        let events = events();
        assert_eq!(count_events(&events, Event::QueueUnset(RX_QUEUE_IDX)), 1);
        assert_eq!(count_events(&events, Event::QueueUnset(TX_QUEUE_IDX)), 1);
        assert_eq!(count_events(&events, Event::QueueUnset(EVENT_QUEUE_IDX)), 1);
        let reset = position(&events, Event::Reset);
        let last_unset = last_position(&events, Event::QueueUnset(EVENT_QUEUE_IDX));
        let first_unshare = position(&events, Event::Unshare);
        let last_unshare = last_position(&events, Event::Unshare);
        let transport_drop = position(&events, Event::TransportDrop);
        assert!(last_unset < reset, "{events:?}");
        assert!(reset < first_unshare, "{events:?}");
        assert!(last_unshare < transport_drop, "{events:?}");
        assert_eq!(count_events(&events, Event::Unshare), QUEUE_SIZE);
        assert_eq!(
            state.lock().unwrap().status_history.last().copied(),
            Some(DeviceStatus::empty())
        );
    }

    #[test]
    fn completed_rx_buffer_is_reused_and_released_once() {
        reset_harness();
        let (transport, state) = RecordingTransport::new(0);
        let mut socket =
            VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).unwrap();

        let header = VirtioVsockHdr {
            op: VirtioVsockOp::CreditUpdate.into(),
            ..Default::default()
        };
        state
            .lock()
            .unwrap()
            .write_to_queue::<QUEUE_SIZE>(RX_QUEUE_IDX, header.as_bytes());

        assert_eq!(socket.poll(|_event, _body| Ok(None)), Ok(None));
        let counts_after_poll = counts();
        assert_eq!(counts_after_poll.unshare, 1);
        assert_eq!(counts_after_poll.share, QUEUE_SIZE + 1);

        drop(socket);
        let counts = counts();
        assert_eq!(counts.share, QUEUE_SIZE + 1);
        assert_eq!(counts.unshare, QUEUE_SIZE + 1);
        assert_eq!(counts.dealloc, 6);
    }

    #[test]
    fn rx_fill_failure_resets_before_releasing_published_buffers() {
        for fail_at in [1, 65] {
            reset_harness();
            let (transport, state) = RecordingTransport::new(0);
            fail_share_at(fail_at);

            let result = VirtIOSocket::<CountingHal, RecordingTransport>::new(transport);
            assert!(result.is_err(), "fill should fail at share {fail_at}");

            let counts = counts();
            assert_eq!(counts.share, fail_at);
            assert_eq!(counts.unshare, fail_at - 1);
            assert_eq!(counts.alloc, 6);
            assert_eq!(counts.dealloc, 6);

            let events = events();
            let reset = position(&events, Event::Reset);
            if fail_at == 1 {
                // Nothing was published, so a correct implementation has
                // nothing to unshare before the partially built queues and
                // transport are dropped.
                assert_eq!(count_events(&events, Event::Unshare), 0, "{events:?}");
            } else {
                // Every published buffer was unshared after the reset which
                // stopped the device, and before the transport was dropped.
                assert!(reset < position(&events, Event::Unshare), "{events:?}");
                assert!(
                    last_position(&events, Event::Unshare)
                        < position(&events, Event::TransportDrop),
                    "{events:?}"
                );
            }
            assert!(reset < position(&events, Event::TransportDrop), "{events:?}");
            assert_eq!(
                state.lock().unwrap().status_history.last().copied(),
                Some(DeviceStatus::empty())
            );
        }
    }

    #[test]
    fn indirect_tx_unshares_every_mapping_once() {
        reset_harness();
        let features = (Feature::RING_INDIRECT_DESC | Feature::VERSION_1).bits();
        let (mut transport, _state) = RecordingTransport::new(features);
        transport.complete_tx = true;
        let mut socket =
            VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).unwrap();
        let mut connection = connection();

        socket.send(&[0xaa; 4], &mut connection).unwrap();

        // One indirect list plus the header and body mappings.
        let counts_after_send = counts();
        assert_eq!(counts_after_send.share, QUEUE_SIZE + 3);
        assert_eq!(counts_after_send.unshare, 3);

        drop(socket);
        let counts_after_drop = counts();
        assert_eq!(counts_after_drop.share, QUEUE_SIZE + 3);
        assert_eq!(counts_after_drop.unshare, QUEUE_SIZE + 3);
        assert_eq!(counts_after_drop.dealloc, 6);
    }

    #[test]
    fn wrong_token_stops_device_and_releases_every_outstanding_buffer() {
        reset_harness();
        let (mut transport, _state) = RecordingTransport::new(0);
        transport.complete_tx = true;
        transport.corrupt_tx_id = Some(1);
        let mut socket =
            VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).unwrap();
        let mut connection = connection();

        assert_eq!(
            socket.send(&[0xaa; 4], &mut connection),
            Err(Error::IoError)
        );

        let counts_after_failure = counts();
        assert_eq!(counts_after_failure.share, QUEUE_SIZE + 2);
        assert_eq!(counts_after_failure.unshare, QUEUE_SIZE + 2);

        let events_after_failure = events();
        assert_eq!(count_events(&events_after_failure, Event::Reset), 1);
        assert!(
            position(&events_after_failure, Event::Reset)
                < position(&events_after_failure, Event::Unshare),
            "{events_after_failure:?}"
        );

        let event_count = events_after_failure.len();
        let count_snapshot = counts();
        assert_eq!(socket.poll(|_event, _body| Ok(None)), Err(Error::IoError));
        assert_eq!(
            socket.send(&[0xbb; 4], &mut connection),
            Err(Error::IoError)
        );
        assert_eq!(counts(), count_snapshot);
        assert_eq!(events().len(), event_count);

        drop(socket);
        assert_eq!(counts().unshare, QUEUE_SIZE + 2);

        let events_after_drop = events();
        assert_eq!(count_events(&events_after_drop, Event::Reset), 2);
        assert!(
            last_position(&events_after_drop, Event::Unshare)
                < position(&events_after_drop, Event::TransportDrop),
            "{events_after_drop:?}"
        );
    }

    #[test]
    fn unpublished_tx_failure_does_not_stop_the_device() {
        reset_harness();
        let (mut transport, _state) = RecordingTransport::new(0);
        transport.complete_tx = true;
        let mut socket =
            VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).unwrap();
        let mut connection = connection();

        fail_share_at(QUEUE_SIZE + 1);
        assert_eq!(
            socket.send(&[0xaa; 4], &mut connection),
            Err(Error::DmaError)
        );
        assert_eq!(count_events(&events(), Event::Reset), 0);
        assert_eq!(counts().unshare, 0);

        clear_failures();
        socket.send(&[0xbb; 4], &mut connection).unwrap();
        assert_eq!(counts().share, QUEUE_SIZE + 3);
        assert_eq!(counts().unshare, 2);

        drop(socket);
        assert_eq!(counts().unshare, QUEUE_SIZE + 2);
        assert_eq!(counts().dealloc, 6);
    }

    #[test]
    fn ring_allocation_failure_releases_partial_queue() {
        reset_harness();
        let (transport, _state) = RecordingTransport::new(0);
        fail_alloc_at(2);

        assert!(VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).is_err());

        let counts = counts();
        assert_eq!(counts.alloc, 2);
        assert_eq!(counts.dealloc, 1);
        assert_eq!(counts.share, 0);
        assert_eq!(counts.unshare, 0);
        assert_eq!(count_events(&events(), Event::Reset), 1);
    }

    #[test]
    fn next_queue_allocation_failure_releases_existing_queues_after_reset() {
        reset_harness();
        let (transport, state) = RecordingTransport::new(0);
        fail_alloc_at(5);

        assert!(VirtIOSocket::<CountingHal, RecordingTransport>::new(transport).is_err());

        let counts = counts();
        assert_eq!(counts.alloc, 5);
        assert_eq!(counts.dealloc, 4);
        assert_eq!(counts.share, 0);
        assert_eq!(counts.unshare, 0);

        let events = events();
        assert_eq!(count_events(&events, Event::Reset), 1);
        assert!(
            position(&events, Event::Reset) < position(&events, Event::Dealloc),
            "{events:?}"
        );
        assert!(
            last_position(&events, Event::Dealloc) < position(&events, Event::TransportDrop),
            "{events:?}"
        );
        assert_eq!(
            state.lock().unwrap().status_history.last().copied(),
            Some(DeviceStatus::empty())
        );
    }

    #[test]
    fn tx_primitives_submit_and_complete_encoded_packet() {
        let (mut socket, state) = test_socket();
        let connection = test_connection(1024);
        let mut packet = vec![0; VSOCK_HEADER_SIZE];
        packet.extend_from_slice(b"hello");
        socket
            .prepare_tx_packet(
                VsockTxPacketKind::Data { payload_len: 5 },
                &connection,
                &mut packet,
            )
            .unwrap();

        // SAFETY: `packet` remains alive and unmodified until its matching completion.
        let token = unsafe { socket.transmit_begin(&packet) }.unwrap();
        assert_eq!(socket.available_tx_descriptors(), QUEUE_SIZE - 1);
        assert_eq!(socket.poll_transmit(), None);

        let transmitted = state
            .lock()
            .unwrap()
            .read_from_queue::<QUEUE_SIZE>(TX_QUEUE_IDX);
        assert_eq!(transmitted, packet);
        assert_eq!(socket.poll_transmit(), Some(token));

        // SAFETY: `packet` is the exact buffer submitted above and `token` is the next completed
        // descriptor.
        unsafe { socket.transmit_complete(token, &packet) }.unwrap();
        assert_eq!(socket.available_tx_descriptors(), QUEUE_SIZE);
    }

    #[test]
    fn shutdown_packet_encodes_flags() {
        let (socket, state) = test_socket();
        let connection = test_connection(1024);
        let mut packet = vec![0; VSOCK_HEADER_SIZE];
        socket
            .prepare_tx_packet(
                VsockTxPacketKind::Shutdown(StreamShutdown::SEND),
                &connection,
                &mut packet,
            )
            .unwrap();
        assert_eq!(
            VirtioVsockHdr::read_from_bytes(&packet).unwrap(),
            VirtioVsockHdr {
                op: VirtioVsockOp::Shutdown.into(),
                flags: StreamShutdown::SEND.bits().into(),
                src_cid: 66u64.into(),
                dst_cid: 2u64.into(),
                src_port: 4321u32.into(),
                dst_port: 1234u32.into(),
                buf_alloc: 1024u32.into(),
                ..Default::default()
            }
        );
        assert!(!state.lock().unwrap().queues[TX_QUEUE_IDX as usize]
            .notified
            .load(core::sync::atomic::Ordering::Relaxed));
    }
}
