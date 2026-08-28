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

use alloc::vec::Vec;

use driver_base::{Device, DeviceKind, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::socket::{
        ConnectionInfo, VirtIOSocket, VsockAddr as VirtioVsockAddr, VsockEvent, VsockEventType,
    },
    transport::Transport,
};
use vsock::{VsockConnectionInfo, VsockDevice, VsockTransportEvent, VsockTransportEventKind};

use crate::as_driver_error;

/// Per-vsock RX virtqueue buffer size.
const RX_BUFFER_SIZE: usize = 8192;

/// Low-level VirtIO vsock device.
///
/// Implements [`VsockDevice`] for the underlying [`VirtIOSocket`]. This is a
/// packet-oriented transport: it does not keep connection tables, listen sets,
/// accept queues, or per-connection receive buffers. Those are owned by the
/// connection manager in `net/knet`.
pub struct VirtIoVsockDev<H: Hal, T: Transport, const RX_BUF_SIZE: usize = RX_BUFFER_SIZE> {
    inner: SpinNoIrq<VirtIOSocket<H, T, RX_BUF_SIZE>>,
    guest_cid: u64,
    irq: Option<usize>,
}

// SAFETY: VirtIoVsockDev serializes all access to the inner VirtIOSocket
// through its own SpinNoIrq lock. The inner type is not auto Send/Sync due to
// PhantomData, but it is safe to transfer across threads and share behind that
// lock.
unsafe impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Send
    for VirtIoVsockDev<H, T, RX_BUF_SIZE>
{
}
// SAFETY: same as the Send impl above: the inner VirtIOSocket is only accessed
// through the SpinNoIrq lock, so it can be shared across threads.
unsafe impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Sync
    for VirtIoVsockDev<H, T, RX_BUF_SIZE>
{
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> Drop for VirtIoVsockDev<H, T, RX_BUF_SIZE> {
    fn drop(&mut self) {
        // The inner VirtIOSocket already clears virtqueue pointers on drop.
    }
}

impl<H: Hal, T: Transport, const RX_BUF_SIZE: usize> VirtIoVsockDev<H, T, RX_BUF_SIZE> {
    /// Create a new raw VirtIO vsock transport and initialize the device.
    ///
    /// Returns an error if the device fails to initialize (feature
    /// negotiation, queue allocation, etc.).
    pub fn try_new(transport: T, irq: Option<usize>) -> DriverResult<Self> {
        let inner = VirtIOSocket::<H, T, RX_BUF_SIZE>::new(transport).map_err(as_driver_error)?;
        let guest_cid = inner.guest_cid();
        Ok(Self {
            inner: SpinNoIrq::new(inner),
            guest_cid,
            irq,
        })
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

    fn irq(&self) -> Option<usize> {
        self.irq
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
        let ci = Self::to_connection_info(info);
        self.inner.lock().connect(&ci).map_err(as_driver_error)
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        let ci = Self::to_connection_info(info);
        self.inner.lock().accept(&ci).map_err(as_driver_error)
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        let ci = Self::to_connection_info(info);
        self.inner.lock().force_close(&ci).map_err(as_driver_error)
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize> {
        let mut ci = Self::to_connection_info(info);
        self.inner
            .lock()
            .send(buf, &mut ci)
            .map_err(as_driver_error)?;
        Ok(buf.len())
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        let ci = Self::to_connection_info(info);
        self.inner.lock().shutdown(&ci).map_err(as_driver_error)
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        let ci = Self::to_connection_info(info);
        self.inner
            .lock()
            .credit_update(&ci)
            .map_err(as_driver_error)
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        let mut got_event = false;
        // Remove scratch SpinNoIrq field from VirtIoVsockDev that caused
        // can_preempt(2) panic; revert poll_event to local Vec
        let mut body = Vec::new();
        let mut transport_event = None;

        // Poll the inner VirtIOSocket once. The callback is called while the
        // inner lock is held, so we copy the event and payload out.
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
