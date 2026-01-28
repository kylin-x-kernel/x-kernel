use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio_drivers::{
    Hal,
    device::socket::{
        VirtIOSocket, VsockAddr, VsockConnectionManager as InnerDev, VsockEvent, VsockEventType,
    },
    transport::Transport,
};
use vsock::{VsockConnId, VsockDriverEventType, VsockDriverOps};

use crate::as_driver_error;

/// Default buffer size for VirtIO socket device (32KB).
const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

/// The VirtIO socket device driver.
pub struct VirtIoSocketDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoSocketDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoSocketDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoSocketDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let virtio_socket = VirtIOSocket::<H, _>::new(transport).map_err(as_driver_error)?;
        Ok(Self {
            inner: InnerDev::new_with_capacity(virtio_socket, DEFAULT_BUFFER_SIZE as u32),
        })
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIoSocketDev<H, T> {
    fn name(&self) -> &str {
        "virtio-socket"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Vsock
    }
}

fn extract_addr_and_port(cid: VsockConnId) -> (VsockAddr, u32) {
    (
        VsockAddr {
            cid: cid.peer_addr.cid as _,
            port: cid.peer_addr.port as _,
        },
        cid.local_port,
    )
}

impl<H: Hal, T: Transport> VsockDriverOps for VirtIoSocketDev<H, T> {
    fn guest_cid(&self) -> u64 {
        self.inner.guest_cid()
    }

    fn listen(&mut self, src_port: u32) {
        self.inner.listen(src_port)
    }

    fn connect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        self.inner
            .connect(peer_addr, src_port)
            .map_err(as_driver_error)
    }

    fn send(&mut self, cid: VsockConnId, buf: &[u8]) -> DriverResult<usize> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        match self.inner.send(peer_addr, src_port, buf) {
            Ok(()) => Ok(buf.len()),
            Err(e) => Err(as_driver_error(e)),
        }
    }

    fn recv(&mut self, cid: VsockConnId, buf: &mut [u8]) -> DriverResult<usize> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        let res = self
            .inner
            .recv(peer_addr, src_port, buf)
            .map_err(as_driver_error);
        let _ = self.inner.update_credit(peer_addr, src_port);
        res
    }

    fn recv_avail(&mut self, cid: VsockConnId) -> DriverResult<usize> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        self.inner
            .recv_buffer_available_bytes(peer_addr, src_port)
            .map_err(as_driver_error)
    }

    fn disconnect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        self.inner
            .shutdown(peer_addr, src_port)
            .map_err(as_driver_error)
    }

    fn abort(&mut self, cid: VsockConnId) -> DriverResult<()> {
        let (peer_addr, src_port) = extract_addr_and_port(cid);
        self.inner
            .force_close(peer_addr, src_port)
            .map_err(as_driver_error)
    }

    fn poll_event(&mut self) -> DriverResult<Option<VsockDriverEventType>> {
        match self.inner.poll() {
            Ok(None) => {
                // no event
                Ok(None)
            }
            Ok(Some(event)) => {
                // translate event
                let result = translate_virtio_event(event, &mut self.inner)?;
                Ok(Some(result))
            }
            Err(e) => {
                // error
                Err(as_driver_error(e))
            }
        }
    }
}

fn translate_virtio_event<H: Hal, T: Transport>(
    event: VsockEvent,
    _inner: &mut InnerDev<H, T>,
) -> DriverResult<VsockDriverEventType> {
    let cid = VsockConnId {
        peer_addr: vsock::VsockAddr {
            cid: event.source.cid as _,
            port: event.source.port as _,
        },
        local_port: event.destination.port,
    };

    match event.event_type {
        VsockEventType::ConnectionRequest => Ok(VsockDriverEventType::ConnectionRequest(cid)),
        VsockEventType::Connected => Ok(VsockDriverEventType::Connected(cid)),
        VsockEventType::Received { length } => Ok(VsockDriverEventType::Received(cid, length)),
        VsockEventType::Disconnected { reason: _ } => Ok(VsockDriverEventType::Disconnected(cid)),
        _ => Ok(VsockDriverEventType::Unknown),
    }
}
