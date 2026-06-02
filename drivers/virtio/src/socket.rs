// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO vsock driver adapter.
use driver_base::{Device, DeviceKind, DriverResult};
use virtio_drivers::{
    Hal,
    device::socket::{
        VirtIOSocket, VsockAddr, VsockConnectionManager as InnerDev, VsockEvent, VsockEventType,
    },
    transport::Transport,
};
use vsock::{VsockConnId, VsockDevice, VsockDriverEventType};

use crate::as_driver_error;

/// Default buffer size for VirtIO socket device (32KB).
const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

struct ConnectionArgs {
    peer_addr: VsockAddr,
    host_port: u32,
}

impl ConnectionArgs {
    fn from_conn_id(conn_id: VsockConnId) -> Self {
        Self {
            peer_addr: VsockAddr {
                cid: conn_id.peer_addr.cid as _,
                port: conn_id.peer_addr.port as _,
            },
            host_port: conn_id.local_port,
        }
    }
}

/// The VirtIO socket device driver.
///
/// Wraps [`VsockConnectionManager`] from `virtio-drivers` and implements the
/// [`VsockDevice`] trait, providing connection-oriented socket communication
/// between the guest and host via the VirtIO vsock transport.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut vsock = VirtIoSocketDev::<HalImpl, _>::try_new(transport)?;
/// vsock.listen(1234);
/// let event = vsock.poll_event()?;
/// ```
pub struct VirtIoSocketDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
}

// SAFETY: VirtIoSocketDev accesses the device exclusively through &mut self.
// The inner VsockConnectionManager is not auto Send/Sync due to PhantomData,
// but it is safe to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport> Send for VirtIoSocketDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoSocketDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoSocketDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if the VirtIO socket device fails to initialize
    /// (e.g. feature negotiation failure, queue allocation failure).
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let virtio_socket = Self::open_socket(transport)?;
        Ok(Self {
            inner: InnerDev::new_with_capacity(virtio_socket, DEFAULT_BUFFER_SIZE as u32),
        })
    }

    fn open_socket(transport: T) -> DriverResult<VirtIOSocket<H, T>> {
        VirtIOSocket::<H, _>::new(transport).map_err(as_driver_error)
    }

    fn translate_event(event: VsockEvent) -> VsockDriverEventType {
        let connection = VsockConnId {
            peer_addr: vsock::VsockAddr {
                cid: event.source.cid as _,
                port: event.source.port as _,
            },
            local_port: event.destination.port,
        };

        match event.event_type {
            VsockEventType::ConnectionRequest => {
                VsockDriverEventType::ConnectionRequest(connection)
            }
            VsockEventType::Connected => VsockDriverEventType::Connected(connection),
            VsockEventType::Received { length } => {
                VsockDriverEventType::Received(connection, length)
            }
            VsockEventType::Disconnected { .. } => VsockDriverEventType::Disconnected(connection),
            VsockEventType::CreditUpdate => VsockDriverEventType::CreditUpdate(connection),
            _ => VsockDriverEventType::Unknown,
        }
    }

    fn refresh_credit(&mut self, connection: &ConnectionArgs) {
        let _ = self
            .inner
            .update_credit(connection.peer_addr, connection.host_port);
    }
}

impl<H: Hal, T: Transport> Device for VirtIoSocketDev<H, T> {
    fn name(&self) -> &str {
        "virtio-socket"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Vsock
    }
}

impl<H: Hal, T: Transport> VsockDevice for VirtIoSocketDev<H, T> {
    fn guest_cid(&self) -> u64 {
        self.inner.guest_cid()
    }

    fn listen(&mut self, src_port: u32) {
        self.inner.listen(src_port)
    }

    fn connect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let connection = ConnectionArgs::from_conn_id(cid);
        self.inner
            .connect(connection.peer_addr, connection.host_port)
            .map_err(as_driver_error)
    }

    fn send(&mut self, cid: VsockConnId, buf: &[u8]) -> DriverResult<usize> {
        let connection = ConnectionArgs::from_conn_id(cid);
        match self
            .inner
            .send(connection.peer_addr, connection.host_port, buf)
        {
            Ok(()) => Ok(buf.len()),
            Err(e) => Err(as_driver_error(e)),
        }
    }

    fn recv(&mut self, cid: VsockConnId, buf: &mut [u8]) -> DriverResult<usize> {
        let connection = ConnectionArgs::from_conn_id(cid);
        let res = self
            .inner
            .recv(connection.peer_addr, connection.host_port, buf)
            .map_err(as_driver_error);
        self.refresh_credit(&connection);
        res
    }

    fn recv_avail(&mut self, cid: VsockConnId) -> DriverResult<usize> {
        let connection = ConnectionArgs::from_conn_id(cid);
        self.inner
            .recv_buffer_available_bytes(connection.peer_addr, connection.host_port)
            .map_err(as_driver_error)
    }

    fn disconnect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let connection = ConnectionArgs::from_conn_id(cid);
        self.inner
            .shutdown(connection.peer_addr, connection.host_port)
            .map_err(as_driver_error)
    }

    fn abort(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let connection = ConnectionArgs::from_conn_id(cid);
        self.inner
            .force_close(connection.peer_addr, connection.host_port)
            .map_err(as_driver_error)
    }

    fn poll_event(&mut self) -> DriverResult<Option<VsockDriverEventType>> {
        let Some(event) = self.inner.poll().map_err(as_driver_error)? else {
            return Ok(None);
        };
        Ok(Some(Self::translate_event(event)))
    }
}
