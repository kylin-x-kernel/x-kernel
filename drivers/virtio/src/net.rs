// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO network driver adapter.
use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
use driver_net::{MacAddress, NetBuf, NetBufBox, NetBufHandle, NetBufPool, NetDriverOps};
use kspin::SpinNoIrq;
use virtio_drivers::{Hal, device::net::VirtIONetRaw as InnerDev, transport::Transport};

use crate::as_driver_error;

const NET_BUF_LEN: usize = 1526;

trait VirtIoNetIrqAck: Send + Sync {
    fn ack_interrupt(&self) -> bool;
}

struct VirtIoNetIrqHandle<H: Hal, T: Transport, const QS: usize> {
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
}

unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetIrqHandle<H, T, QS> {}
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetIrqHandle<H, T, QS> {}

impl<H: Hal, T: Transport, const QS: usize> VirtIoNetIrqAck for VirtIoNetIrqHandle<H, T, QS> {
    fn ack_interrupt(&self) -> bool {
        !self.inner.lock().ack_interrupt().is_empty()
    }
}

static NET_IRQ_HANDLES: SpinNoIrq<Vec<Arc<dyn VirtIoNetIrqAck>>> = SpinNoIrq::new(Vec::new());
static REGISTERED_NET_IRQS: SpinNoIrq<BTreeSet<usize>> = SpinNoIrq::new(BTreeSet::new());

fn handle_virtio_net_irq() {
    let handles = NET_IRQ_HANDLES.lock();
    handles.iter().for_each(|irq_handle| {
        let _ = irq_handle.ack_interrupt();
    });
}

fn register_virtio_net_irq<H: Hal + 'static, T: Transport + 'static, const QS: usize>(
    irq: usize,
    inner: &Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
) -> DriverResult {
    NET_IRQ_HANDLES
        .lock()
        .push(Arc::new(VirtIoNetIrqHandle::<H, T, QS> {
            inner: inner.clone(),
        }));

    if REGISTERED_NET_IRQS.lock().insert(irq) && !khal::irq::register(irq, handle_virtio_net_irq) {
        NET_IRQ_HANDLES.lock().pop();
        REGISTERED_NET_IRQS.lock().remove(&irq);
        return Err(DriverError::ResourceBusy);
    }

    Ok(())
}

/// The VirtIO network device driver.
///
/// `QS` is the VirtIO queue size.
pub struct VirtIoNetDev<H: Hal, T: Transport, const QS: usize> {
    rx_buffers: [Option<NetBufBox>; QS],
    tx_buffers: [Option<NetBufBox>; QS],
    free_tx_bufs: Vec<NetBufBox>,
    buf_pool: Arc<NetBufPool>,
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
    irq: Option<usize>,
}

unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetDev<H, T, QS> {}
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetDev<H, T, QS> {}

impl<H: Hal + 'static, T: Transport + 'static, const QS: usize> VirtIoNetDev<H, T, QS> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    pub fn try_new(transport: T, irq: Option<usize>) -> DriverResult<Self> {
        // 0. Create a new driver instance.
        const NONE_BUF: Option<NetBufBox> = None;
        let inner = Arc::new(SpinNoIrq::new(
            InnerDev::new(transport).map_err(as_driver_error)?,
        ));
        let rx_buffers = [NONE_BUF; QS];
        let tx_buffers = [NONE_BUF; QS];
        let buf_pool = NetBufPool::new(2 * QS, NET_BUF_LEN)?;
        let free_tx_bufs = Vec::with_capacity(QS);

        let mut dev = Self {
            rx_buffers,
            inner,
            tx_buffers,
            free_tx_bufs,
            buf_pool,
            irq,
        };

        // 1. Fill all rx buffers.
        for (i, rx_buf_place) in dev.rx_buffers.iter_mut().enumerate() {
            let mut rx_buf = dev.buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // Safe because the buffer lives as long as the queue.
            let token = unsafe {
                dev.inner
                    .lock()
                    .receive_begin(rx_buf.buffer_mut())
                    .map_err(as_driver_error)?
            };
            assert_eq!(token, i as u16);
            *rx_buf_place = Some(rx_buf);
        }

        // 2. Allocate all tx buffers.
        for _ in 0..QS {
            let mut tx_buf = dev.buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // Fill header
            let hdr_len = dev
                .inner
                .lock()
                .fill_buffer_header(tx_buf.buffer_mut())
                .or(Err(DriverError::InvalidInput))?;
            tx_buf.set_hdr_len(hdr_len);
            dev.free_tx_bufs.push(tx_buf);
        }

        if let Some(irq) = dev.irq {
            register_virtio_net_irq(irq, &dev.inner)?;
        }

        // 3. Return the driver instance.
        Ok(dev)
    }
}

impl<H: Hal, T: Transport, const QS: usize> DriverOps for VirtIoNetDev<H, T, QS> {
    fn name(&self) -> &str {
        "virtio-net"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Net
    }

    fn irq(&self) -> Option<usize> {
        self.irq
    }
}

impl<H: Hal, T: Transport, const QS: usize> NetDriverOps for VirtIoNetDev<H, T, QS> {
    #[inline]
    fn mac(&self) -> MacAddress {
        MacAddress(self.inner.lock().mac_address())
    }

    #[inline]
    fn can_tx(&self) -> bool {
        !self.free_tx_bufs.is_empty() && self.inner.lock().can_send()
    }

    #[inline]
    fn can_rx(&self) -> bool {
        self.inner.lock().poll_receive().is_some()
    }

    #[inline]
    fn rx_queue_len(&self) -> usize {
        QS
    }

    #[inline]
    fn tx_queue_len(&self) -> usize {
        QS
    }

    fn recycle_rx(&mut self, rx_buf: NetBufHandle) -> DriverResult {
        let mut rx_buf = unsafe { NetBuf::from_handle(rx_buf) };
        // Safe because we take the ownership of `rx_buf` back to `rx_buffers`,
        // it lives as long as the queue.
        let new_token = unsafe {
            self.inner
                .lock()
                .receive_begin(rx_buf.buffer_mut())
                .map_err(as_driver_error)?
        };
        // `rx_buffers[new_token]` is expected to be `None` since it was taken
        // away at `Self::recv()` and has not been added back.
        if self.rx_buffers[new_token as usize].is_some() {
            return Err(DriverError::BadState);
        }
        self.rx_buffers[new_token as usize] = Some(rx_buf);
        Ok(())
    }

    fn recycle_tx(&mut self) -> DriverResult {
        loop {
            let token = {
                let mut inner = self.inner.lock();
                inner.poll_transmit()
            };
            let Some(token) = token else {
                break;
            };
            let tx_buf = self.tx_buffers[token as usize]
                .take()
                .ok_or(DriverError::BadState)?;
            unsafe {
                self.inner
                    .lock()
                    .transmit_complete(token, tx_buf.frame())
                    .map_err(as_driver_error)?;
            }
            // Recycle the buffer.
            self.free_tx_bufs.push(tx_buf);
        }
        Ok(())
    }

    fn send(&mut self, tx_buf: NetBufHandle) -> DriverResult {
        // 0. prepare tx buffer.
        let tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
        // 1. send payload.
        let token = unsafe {
            self.inner
                .lock()
                .transmit_begin(tx_buf.frame())
                .map_err(as_driver_error)?
        };
        self.tx_buffers[token as usize] = Some(tx_buf);
        Ok(())
    }

    fn recv(&mut self) -> DriverResult<NetBufHandle> {
        self.inner.lock().ack_interrupt();
        let token = {
            let inner = self.inner.lock();
            inner.poll_receive()
        };
        if let Some(token) = token {
            let mut rx_buf = self.rx_buffers[token as usize]
                .take()
                .ok_or(DriverError::BadState)?;
            // Safe because the buffer lives as long as the queue.
            let (hdr_len, pkt_len) = unsafe {
                self.inner
                    .lock()
                    .receive_complete(token, rx_buf.buffer_mut())
                    .map_err(as_driver_error)?
            };
            rx_buf.set_hdr_len(hdr_len);
            rx_buf.set_payload_len(pkt_len);

            Ok(rx_buf.into_handle())
        } else {
            Err(DriverError::WouldBlock)
        }
    }

    fn alloc_tx_buf(&mut self, size: usize) -> DriverResult<NetBufHandle> {
        // 0. Allocate a buffer from the queue.
        let mut net_buf = self.free_tx_bufs.pop().ok_or(DriverError::NoMemory)?;
        let pkt_len = size;

        // 1. Check if the buffer is large enough.
        let hdr_len = net_buf.hdr_len();
        if hdr_len + pkt_len > net_buf.capacity() {
            return Err(DriverError::InvalidInput);
        }
        net_buf.set_payload_len(pkt_len);

        // 2. Return the buffer.
        Ok(net_buf.into_handle())
    }
}
