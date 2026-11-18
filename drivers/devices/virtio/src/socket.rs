// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw VirtIO vsock transport adapter.
//!
//! This adapter wraps [`VirtIOSocket`] from the `virtio-drivers` crate and
//! exposes only packet-level virtio-vsock operations: connect, accept, send,
//! receive-event poll, and credit control. It does not keep connection tables,
//! listen sets, accept queues, or per-connection receive buffers. Those are
//! owned by the connection manager in `net/knet`.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::{SpinNoIrq, SpinNoPreempt};
use virtio_drivers::{
    Error as VirtioError, Hal,
    device::socket::{
        ConnectionInfo, QUEUE_SIZE, StreamShutdown, VSOCK_HEADER_SIZE, VirtIOSocket,
        VsockAddr as VirtioVsockAddr, VsockEvent, VsockEventType, VsockTxPacketKind,
    },
    transport::Transport,
};
use vsock::{
    TX_MAX_PAYLOAD_SIZE, VsockConnectionInfo, VsockDevice, VsockTransportEvent,
    VsockTransportEventKind, VsockTxBuf, VsockTxBufPool,
};

use crate::as_driver_error;

/// Per-vsock RX virtqueue buffer size.
///
/// This intentionally does not mirror Linux's 4 KiB RX SKB allocation. The
/// vsock-TIPC bridge currently derives TIPC message boundaries from RX events,
/// so a 4 KiB buffer adds vhost splits for records larger than 4 KiB. Keep the
/// 8 KiB buffer to preserve the existing message-boundary behavior.
const RX_BUFFER_SIZE: usize = 8192;

/// Complete TX buffer length, including the virtio-vsock header.
const TX_BUF_SIZE: usize = VSOCK_HEADER_SIZE + TX_MAX_PAYLOAD_SIZE;
/// Keep one buffer slot available for credit and connection-control packets.
const TX_CONTROL_RESERVED_BUFS: usize = 1;
/// Keep one descriptor available for credit and connection-control packets.
const TX_CONTROL_RESERVED_DESCRIPTORS: usize = 1;

/// A control packet deferred because the TX pool was saturated.
///
/// When [`send_control_packet`] cannot obtain a free TX buffer or submit to
/// the virtqueue, the packet is stored here and retried on the next
/// [`recycle_tx`] call.
#[derive(Clone)]
struct ControlEnqueue {
    info: ConnectionInfo,
    kind: VsockTxPacketKind,
}

/// TX buffer bookkeeping for in-flight and free vsock buffers.
///
/// This state is only touched from task context, so it is guarded by `SpinNoPreempt` like the
/// network adapter's `NetBufState`. A future IRQ path that reclaims TX must move this lock to
/// `SpinNoIrq` or defer reclamation to task context.
struct VsockTxBufState<const QS: usize> {
    /// Control packets deferred when the TX pool was saturated. Flushed at the
    /// start of [`recycle_tx`] before reclaiming completed descriptors.
    pending_controls: VecDeque<ControlEnqueue>,
    tx_bufs: [Option<VsockTxBuf>; QS],
    free_tx_bufs: Vec<VsockTxBuf>,
}

impl<const QS: usize> VsockTxBufState<QS> {
    fn new(pool: &Arc<VsockTxBufPool>) -> DriverResult<Self> {
        let mut free_tx_bufs = Vec::new();
        free_tx_bufs
            .try_reserve_exact(QS)
            .map_err(|_| DriverError::NoMemory)?;
        for _ in 0..QS {
            free_tx_bufs.push(pool.alloc_buf().ok_or(DriverError::NoMemory)?);
        }
        Ok(Self {
            pending_controls: VecDeque::new(),
            tx_bufs: [const { None }; QS],
            free_tx_bufs,
        })
    }

    fn release_buf(&mut self, mut tx_buf: VsockTxBuf) {
        debug_assert!(self.free_tx_bufs.len() < QS);
        tx_buf.reset();
        self.free_tx_bufs.push(tx_buf);
    }
}

/// Low-level VirtIO vsock device.
///
/// Implements [`VsockDevice`] for the underlying [`VirtIOSocket`]. This is a
/// packet-oriented transport: it does not keep connection tables, listen sets,
/// accept queues, or per-connection receive buffers. Those are owned by the
/// connection manager in `net/knet`.
pub struct VirtIoVsockDev<H: Hal, T: Transport, const RX_BUF_SIZE: usize = RX_BUFFER_SIZE> {
    inner: SpinNoIrq<VirtIOSocket<H, T, RX_BUF_SIZE>>,
    /// In-flight and free TX buffers, indexed by descriptor token while submitted.
    tx_bufs: SpinNoPreempt<VsockTxBufState<QUEUE_SIZE>>,
    /// Keeps the buffer storage alive while in-flight buffers reference it.
    #[expect(
        dead_code,
        reason = "CG-024-lint-exceptions: dead_code: Own the TX packet pool for the lifetime of \
                  the vsock device even when no direct field read is needed"
    )]
    tx_buf_pool: Arc<VsockTxBufPool>,
    guest_cid: u64,
}

// SAFETY: VirtIoVsockDev serializes the inner VirtIOSocket with `SpinNoIrq` and its TX buffer
// bookkeeping with `SpinNoPreempt`. Code that needs both locks follows `bufs -> inner`.
unsafe impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Send
    for VirtIoVsockDev<H, T, RX_BUF_SIZE>
{
}
// SAFETY: same as the Send impl above: all mutable state is reached only through those locks.
unsafe impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Sync
    for VirtIoVsockDev<H, T, RX_BUF_SIZE>
{
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Drop for VirtIoVsockDev<H, T, RX_BUF_SIZE> {
    fn drop(&mut self) {
        // Fields drop in declaration order: queues detach before buffers release their pool
        // slots, and pool storage outlives those buffer handles.
    }
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> VirtIoVsockDev<H, T, RX_BUF_SIZE> {
    /// Create a new raw VirtIO vsock transport and initialize the device.
    ///
    /// This adapter is polled and does not acknowledge interrupts. The caller
    /// must arrange transport-level interrupt suppression; PCI probing disables
    /// the function's INTx output before calling this constructor.
    ///
    /// Returns an error if the device fails to initialize (feature
    /// negotiation, queue allocation, TX packet allocation, etc.).
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let inner = VirtIOSocket::<H, T, RX_BUF_SIZE>::new(transport).map_err(as_driver_error)?;
        let guest_cid = inner.guest_cid();
        let buf_pool = VsockTxBufPool::new(QUEUE_SIZE, TX_BUF_SIZE)?;
        let bufs = VsockTxBufState::new(&buf_pool)?;
        Ok(Self {
            inner: SpinNoIrq::new(inner),
            tx_bufs: SpinNoPreempt::new(bufs),
            tx_buf_pool: buf_pool,
            guest_cid,
        })
    }

    fn take_free_tx_buf(&self, reserve_control_buf: bool) -> DriverResult<VsockTxBuf> {
        let mut bufs = self.tx_bufs.lock();
        if reserve_control_buf && bufs.free_tx_bufs.len() <= TX_CONTROL_RESERVED_BUFS {
            return Err(DriverError::WouldBlock);
        }
        let mut tx_buf = bufs.free_tx_bufs.pop().ok_or(DriverError::WouldBlock)?;
        if let Err(error) = tx_buf.set_hdr_len(VSOCK_HEADER_SIZE) {
            bufs.release_buf(tx_buf);
            return Err(error);
        }
        Ok(tx_buf)
    }

    fn release_tx_buf(&self, tx_buf: VsockTxBuf) {
        self.tx_bufs.lock().release_buf(tx_buf);
    }

    fn queue_tx_buf(
        &self,
        mut tx_buf: VsockTxBuf,
        connection_info: &ConnectionInfo,
        kind: VsockTxPacketKind,
    ) -> DriverResult<u16> {
        let mut bufs = self.tx_bufs.lock();
        let mut inner = self.inner.lock();
        if let Err(error) = Self::reclaim_completed_tx_locked(&mut inner, &mut bufs) {
            bufs.release_buf(tx_buf);
            return Err(error);
        }
        if matches!(kind, VsockTxPacketKind::Data { .. })
            && inner.available_tx_descriptors() <= TX_CONTROL_RESERVED_DESCRIPTORS
        {
            bufs.release_buf(tx_buf);
            return Err(DriverError::WouldBlock);
        }
        if let Err(error) = inner.prepare_tx_packet(kind, connection_info, tx_buf.frame_mut()) {
            bufs.release_buf(tx_buf);
            return Err(as_driver_error(error));
        }
        // SAFETY: the pool slot owned by `tx_buf` remains valid and exclusively owned. It is
        // stored in `bufs` before the guards are released and remains there until the
        // matching completion is reclaimed.
        let token = match unsafe { inner.transmit_begin(tx_buf.frame()) } {
            Ok(token) => token,
            Err(VirtioError::QueueFull) => {
                bufs.release_buf(tx_buf);
                return Err(DriverError::WouldBlock);
            }
            Err(error) => {
                bufs.release_buf(tx_buf);
                return Err(as_driver_error(error));
            }
        };
        let index = usize::from(token);
        assert!(
            bufs.tx_bufs[index].is_none(),
            "TX descriptor token already has an in-flight buffer"
        );
        bufs.tx_bufs[index] = Some(tx_buf);
        Ok(token)
    }

    fn reclaim_completed_tx_locked(
        inner: &mut VirtIOSocket<H, T, RX_BUF_SIZE>,
        bufs: &mut VsockTxBufState<QUEUE_SIZE>,
    ) -> DriverResult<usize> {
        let mut reclaimed = 0;
        while let Some(token) = inner.poll_transmit() {
            let index = usize::from(token);
            let packet = bufs.tx_bufs[index].take().ok_or(DriverError::BadState)?;
            // SAFETY: `packet` is the exact buffer stored for `token` by `submit_tx_buf`,
            // and `poll_transmit` returned it as the next completed descriptor.
            if let Err(error) = unsafe { inner.transmit_complete(token, packet.frame()) } {
                bufs.tx_bufs[index] = Some(packet);
                return Err(as_driver_error(error));
            }
            bufs.release_buf(packet);
            reclaimed += 1;
        }
        Ok(reclaimed)
    }

    fn send_tx_buf(
        &self,
        tx_buf: VsockTxBuf,
        connection_info: &ConnectionInfo,
        kind: VsockTxPacketKind,
    ) -> DriverResult<()> {
        self.queue_tx_buf(tx_buf, connection_info, kind)?;
        Ok(())
    }

    /// Sends a header-only control packet, deferring to a retry queue on failure.
    ///
    /// When the TX pool is saturated this function enqueues the control packet
    /// into `pending_controls` instead of returning `WouldBlock`.  The queue is
    /// flushed at the start of the next [`recycle_tx`] call, so control packets
    /// are never lost even when the data pipeline is full.
    fn send_control_packet(
        &self,
        info: &VsockConnectionInfo,
        kind: VsockTxPacketKind,
    ) -> DriverResult<()> {
        let connection_info = Self::to_connection_info(info);
        match self.take_free_tx_buf(false) {
            Ok(tx_buf) => {
                if self.send_tx_buf(tx_buf, &connection_info, kind).is_err() {
                    self.tx_bufs
                        .lock()
                        .pending_controls
                        .push_back(ControlEnqueue {
                            info: connection_info,
                            kind,
                        });
                }
            }
            Err(_) => {
                self.tx_bufs
                    .lock()
                    .pending_controls
                    .push_back(ControlEnqueue {
                        info: connection_info,
                        kind,
                    });
            }
        }
        Ok(())
    }

    /// Drains the pending-control queue, submitting each entry to the virtqueue.
    ///
    /// Stops on the first submission failure and pushes the entry back so it is
    /// retried on the next call.  On failure the buffer is already released by
    /// [`queue_tx_buf`].
    fn flush_pending_controls(&self) {
        while let Some(entry) = {
            let mut bufs = self.tx_bufs.lock();
            bufs.pending_controls.pop_front()
        } {
            let tx_buf = match self.take_free_tx_buf(false) {
                Ok(buf) => buf,
                Err(_) => {
                    self.tx_bufs.lock().pending_controls.push_front(entry);
                    break;
                }
            };
            if self.queue_tx_buf(tx_buf, &entry.info, entry.kind).is_err() {
                self.tx_bufs.lock().pending_controls.push_front(entry);
                break;
            }
        }
    }

    /// Convert [`VsockConnectionInfo`] to the underlying [`ConnectionInfo`].
    ///
    /// This is a trivial field-by-field copy. The `has_pending_credit_request`
    /// field is always `false` because the manager handles credit-request
    /// logic at a higher level.
    fn to_connection_info(info: &VsockConnectionInfo) -> ConnectionInfo {
        let mut ci = ConnectionInfo::new(
            VirtioVsockAddr {
                cid: info.conn_id.peer_addr.cid,
                port: info.conn_id.peer_addr.port,
            },
            info.conn_id.local_port,
        );
        ci.buf_alloc = info.buf_alloc;
        ci.set_fwd_cnt(info.fwd_cnt);
        ci.set_peer_buf_alloc(info.peer_buf_alloc);
        ci.set_peer_fwd_cnt(info.peer_fwd_cnt);
        ci.set_tx_cnt(info.tx_cnt);
        ci
    }

    fn translate_event(event: &VsockEvent) -> VsockTransportEvent {
        let source = vsock::VsockAddr {
            cid: event.source.cid,
            port: event.source.port,
        };
        let destination = vsock::VsockAddr {
            cid: event.destination.cid,
            port: event.destination.port,
        };
        let kind = match event.event_type {
            VsockEventType::ConnectionRequest => VsockTransportEventKind::ConnectionRequest,
            VsockEventType::Connected => VsockTransportEventKind::Connected,
            VsockEventType::Received { length } => VsockTransportEventKind::Received { length },
            VsockEventType::Disconnected { .. } => VsockTransportEventKind::Disconnected,
            VsockEventType::CreditUpdate => VsockTransportEventKind::CreditUpdate {
                buffer_allocation: event.buffer_status.buffer_allocation,
                forward_count: event.buffer_status.forward_count,
            },
            VsockEventType::CreditRequest => VsockTransportEventKind::CreditRequest,
        };
        VsockTransportEvent {
            source,
            destination,
            peer_buf_alloc: event.buffer_status.buffer_allocation,
            peer_fwd_cnt: event.buffer_status.forward_count,
            kind,
        }
    }
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Device for VirtIoVsockDev<H, T, RX_BUF_SIZE> {
    fn name(&self) -> &str {
        "virtio-socket"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Vsock
    }
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> VsockDevice
    for VirtIoVsockDev<H, T, RX_BUF_SIZE>
{
    fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    fn listen(&self, _port: u32) {
        // Raw transport does not track listen ports; the connection manager
        // owns the listen table and decides whether to accept or reject.
    }

    fn unlisten(&self, _port: u32) {
        // Raw transport does not track listen ports.
    }

    fn connect(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(info, VsockTxPacketKind::Request)
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(info, VsockTxPacketKind::Response)
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(info, VsockTxPacketKind::Reset)
    }

    fn alloc_tx_buf(&self) -> DriverResult<VsockTxBuf> {
        self.take_free_tx_buf(true)
    }

    fn submit_tx_buf(&self, info: &VsockConnectionInfo, tx_buf: VsockTxBuf) -> DriverResult<()> {
        let payload_len = tx_buf.payload_len();
        if payload_len == 0 || payload_len > TX_MAX_PAYLOAD_SIZE {
            self.release_tx_buf(tx_buf);
            return Err(DriverError::InvalidInput);
        }
        let peer_free = match usize::try_from(info.peer_free()) {
            Ok(peer_free) => peer_free,
            Err(_) => {
                self.release_tx_buf(tx_buf);
                return Err(DriverError::InvalidInput);
            }
        };
        if peer_free < payload_len {
            self.release_tx_buf(tx_buf);
            return Err(DriverError::WouldBlock);
        }
        let payload_len_u32 = match u32::try_from(payload_len) {
            Ok(payload_len) => payload_len,
            Err(_) => {
                self.release_tx_buf(tx_buf);
                return Err(DriverError::InvalidInput);
            }
        };

        let connection_info = Self::to_connection_info(info);
        self.send_tx_buf(
            tx_buf,
            &connection_info,
            VsockTxPacketKind::Data {
                payload_len: payload_len_u32,
            },
        )
    }

    fn request_credit(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(info, VsockTxPacketKind::CreditRequest)
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(
            info,
            VsockTxPacketKind::Shutdown(StreamShutdown::SEND | StreamShutdown::RECEIVE),
        )
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.send_control_packet(info, VsockTxPacketKind::CreditUpdate)
    }

    fn can_send(&self) -> bool {
        let has_data_packet = {
            let bufs = self.tx_bufs.lock();
            bufs.free_tx_bufs.len() > TX_CONTROL_RESERVED_BUFS
        };
        let has_descriptor =
            self.inner.lock().available_tx_descriptors() > TX_CONTROL_RESERVED_DESCRIPTORS;
        has_data_packet && has_descriptor
    }

    fn recycle_tx(&self) -> DriverResult<usize> {
        self.flush_pending_controls();
        let mut bufs = self.tx_bufs.lock();
        let mut inner = self.inner.lock();
        Self::reclaim_completed_tx_locked(&mut inner, &mut bufs)
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        let mut got_event = false;
        let mut body = Vec::new();
        let mut transport_event = None;

        // Poll the inner VirtIOSocket once. The callback is called while the inner lock is held,
        // so we copy the event and payload out.
        {
            let mut inner = self.inner.lock();
            let result = inner.poll(|event, raw_body| {
                got_event = true;
                body.clear();
                body.extend_from_slice(raw_body);
                transport_event = Some(event);
                Ok(None)
            });
            if let Err(e) = result {
                return Err(as_driver_error(e));
            }
        }

        if !got_event {
            return Ok(false);
        }

        let event = transport_event.expect("event was set when got_event is true");
        let transport_event = Self::translate_event(&event);

        // Release the inner lock before invoking the manager callback so the
        // manager can call back into the transport to send control packets
        // (accept, force_close, credit_update) without deadlocking.
        handler(transport_event, &body)?;

        Ok(true)
    }
}

unittest::path_mod_test!(socket_unittest, "../ut-tests/socket_unittest.rs");
