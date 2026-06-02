// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO network driver adapter.
use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use device_res::{Irq, IrqResource, IrqReturn, IrqTriggerMode};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use driver_net::{MacAddress, NetBuf, NetBufBox, NetBufHandle, NetBufPool, NetDevice};
use kspin::SpinNoIrq;
use virtio_drivers::{Hal, device::net::VirtIONetRaw as InnerDev, transport::Transport};

use crate::as_driver_error;

const NET_BUF_LEN: usize = 1526;

static NEXT_IRQ_HANDLE_ID: AtomicUsize = AtomicUsize::new(0);

trait VirtIoNetIrqAck: Send + Sync {
    fn ack_interrupt(&self) -> bool;
    fn handle_id(&self) -> usize;
    fn irq(&self) -> usize;
}

struct VirtIoNetIrqHandle<H: Hal, T: Transport, const QS: usize> {
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
    id: usize,
    irq: usize,
}

// SAFETY: VirtIoNetIrqHandle only accesses the device through SpinNoIrq,
// which provides mutual exclusion. The inner InnerDev is safe to share
// across threads when protected by a lock.
unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetIrqHandle<H, T, QS> {}
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetIrqHandle<H, T, QS> {}

impl<H: Hal, T: Transport, const QS: usize> VirtIoNetIrqAck for VirtIoNetIrqHandle<H, T, QS> {
    fn ack_interrupt(&self) -> bool {
        !self.inner.lock().ack_interrupt().is_empty()
    }

    fn handle_id(&self) -> usize {
        self.id
    }

    fn irq(&self) -> usize {
        self.irq
    }
}

static NET_IRQ_HANDLES: SpinNoIrq<Vec<Arc<dyn VirtIoNetIrqAck>>> = SpinNoIrq::new(Vec::new());
static REGISTERED_NET_IRQS: SpinNoIrq<BTreeSet<usize>> = SpinNoIrq::new(BTreeSet::new());
/// Keeps the network interrupt registrations alive for the lifetime of the
/// kernel; dropping an [`Irq`] guard would release the handler.
static NET_IRQ_GUARDS: SpinNoIrq<Vec<Irq>> = SpinNoIrq::new(Vec::new());

fn handle_virtio_net_irq() -> IrqReturn {
    let handles = NET_IRQ_HANDLES.lock();
    handles.iter().for_each(|irq_handle| {
        let _ = irq_handle.ack_interrupt();
    });
    IrqReturn::Handled
}

fn register_virtio_net_irq<H: Hal + 'static, T: Transport + 'static, const QS: usize>(
    irq: usize,
    inner: &Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
) -> DriverResult<usize> {
    let handle_id = NEXT_IRQ_HANDLE_ID.fetch_add(1, Ordering::Relaxed);
    NET_IRQ_HANDLES
        .lock()
        .push(Arc::new(VirtIoNetIrqHandle::<H, T, QS> {
            inner: inner.clone(),
            id: handle_id,
            irq,
        }));

    if REGISTERED_NET_IRQS.lock().insert(irq) {
        let resource = IrqResource {
            number: irq,
            trigger: IrqTriggerMode::Unspecified,
        };
        match Irq::request(resource, Arc::new(handle_virtio_net_irq)) {
            Ok(guard) => NET_IRQ_GUARDS.lock().push(guard),
            Err(_) => {
                NET_IRQ_HANDLES.lock().pop();
                REGISTERED_NET_IRQS.lock().remove(&irq);
                return Err(DriverError::ResourceBusy);
            }
        }
    }

    Ok(handle_id)
}

fn unregister_virtio_net_irq(irq: usize, handle_id: usize) {
    NET_IRQ_HANDLES
        .lock()
        .retain(|h| h.handle_id() != handle_id);

    let irq_still_used = NET_IRQ_HANDLES.lock().iter().any(|h| h.irq() == irq);
    if !irq_still_used {
        REGISTERED_NET_IRQS.lock().remove(&irq);
        khal::irq::unregister(irq);
    }
}

/// The VirtIO network device driver.
///
/// Wraps [`VirtIONetRaw`] from `virtio-drivers` and implements the
/// [`NetDevice`] trait, providing packet-level send/receive with buffer
/// pool management and interrupt-driven IRQ acknowledgment.
///
/// `QS` is the VirtIO queue size.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
/// - `QS` - VirtIO queue size (number of descriptors).
///
/// # Concurrency
///
/// The inner device is wrapped in `Arc<SpinNoIrq<...>>` because the IRQ
/// callback runs in interrupt context and must access the device concurrently
/// with normal send/receive operations.
///
/// # Example
///
/// ```ignore
/// let (kind, transport, irq) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut net = VirtIoNetDev::<HalImpl, _, 256>::try_new(transport, Some(irq))?;
/// let tx_buf = net.alloc_tx_buf(1500)?;
/// // ... fill tx_buf and send ...
/// net.send(tx_buf)?;
/// let rx_buf = net.recv()?;
/// ```
pub struct VirtIoNetDev<H: Hal, T: Transport, const QS: usize> {
    rx_buffers: [Option<NetBufBox>; QS],
    tx_buffers: [Option<NetBufBox>; QS],
    free_tx_bufs: Vec<NetBufBox>,
    buf_pool: Arc<NetBufPool>,
    inner: Arc<SpinNoIrq<InnerDev<H, T, QS>>>,
    irq: Option<usize>,
    irq_handle_id: Option<usize>,
}

// SAFETY: VirtIoNetDev's shared state (InnerDev) is protected by SpinNoIrq.
// The buffer arrays are accessed through &mut self exclusively. It is safe
// to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport, const QS: usize> Send for VirtIoNetDev<H, T, QS> {}
unsafe impl<H: Hal, T: Transport, const QS: usize> Sync for VirtIoNetDev<H, T, QS> {}

impl<H: Hal, T: Transport, const QS: usize> Drop for VirtIoNetDev<H, T, QS> {
    fn drop(&mut self) {
        if let (Some(irq), Some(handle_id)) = (self.irq, self.irq_handle_id) {
            unregister_virtio_net_irq(irq, handle_id);
        }
    }
}

impl<H: Hal + 'static, T: Transport + 'static, const QS: usize> VirtIoNetDev<H, T, QS> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Arguments
    ///
    /// - `transport` - The VirtIO transport (MMIO or PCI) for this device.
    /// - `irq` - Optional IRQ number. If provided, an interrupt handler is
    ///   registered to acknowledge device interrupts.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if:
    /// - Device initialization fails (feature negotiation, queue setup).
    /// - Buffer pool allocation fails (`NoMemory`).
    /// - IRQ registration fails (`ResourceBusy`).
    ///
    /// # Panics
    ///
    /// Panics if `receive_begin` returns a token that does not match the
    /// expected sequential index (asserts internal consistency).
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
            irq_handle_id: None,
        };

        // 1. Fill all rx buffers.
        for (i, rx_buf_place) in dev.rx_buffers.iter_mut().enumerate() {
            let mut rx_buf = dev.buf_pool.alloc_boxed().ok_or(DriverError::NoMemory)?;
            // SAFETY: `receive_begin` requires exclusive access to the buffer
            // for the duration it is in the VirtIO queue. The buffer is owned
            // by `rx_buf` and stored in `rx_buffers[i]` immediately after,
            // ensuring it lives as long as the device holds the token.
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
            let handle_id = register_virtio_net_irq(irq, &dev.inner)?;
            dev.irq_handle_id = Some(handle_id);
        }

        // 3. Return the driver instance.
        Ok(dev)
    }
}

impl<H: Hal, T: Transport, const QS: usize> Device for VirtIoNetDev<H, T, QS> {
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

impl<H: Hal, T: Transport, const QS: usize> NetDevice for VirtIoNetDev<H, T, QS> {
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
        // SAFETY: `from_handle` converts a handle back to an owned `NetBuf`.
        // The caller guarantees the handle is valid and not used elsewhere.
        let mut rx_buf = unsafe { NetBuf::from_handle(rx_buf) };
        // SAFETY: `receive_begin` requires exclusive access to the buffer.
        // We own `rx_buf` and will store it in `rx_buffers[new_token]`
        // immediately after, ensuring it lives as long as the device holds
        // the token.
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
            // SAFETY: `transmit_complete` requires that the buffer frame
            // matches the token's previously submitted buffer. We obtained
            // `tx_buf` from `tx_buffers[token]`, which was placed there by
            // `send()`, so the mapping is correct.
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
        // SAFETY: `from_handle` converts a handle back to an owned `NetBuf`.
        // The caller guarantees the handle is valid and not used elsewhere.
        let tx_buf = unsafe { NetBuf::from_handle(tx_buf) };
        // SAFETY: `transmit_begin` requires exclusive access to the buffer
        // frame for the duration it is in the VirtIO queue. We store the
        // buffer in `tx_buffers[token]` immediately after, ensuring it
        // remains valid until `recycle_tx()` retrieves it.
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
            // SAFETY: `receive_complete` requires that the buffer matches the
            // token's previously submitted buffer. We obtained `rx_buf` from
            // `rx_buffers[token]`, which was placed there during `try_new()`
            // or `recycle_rx()`, so the mapping is correct.
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

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    struct MockIrqHandle {
        id: usize,
        irq_num: usize,
    }

    impl VirtIoNetIrqAck for MockIrqHandle {
        fn ack_interrupt(&self) -> bool {
            false
        }

        fn handle_id(&self) -> usize {
            self.id
        }

        fn irq(&self) -> usize {
            self.irq_num
        }
    }

    fn reset_irq_state() {
        NET_IRQ_HANDLES.lock().clear();
        REGISTERED_NET_IRQS.lock().clear();
    }

    #[def_test(serial)]
    fn test_unregister_removes_correct_handle() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 10,
            irq_num: 32,
        }));
        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 11,
            irq_num: 33,
        }));
        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 12,
            irq_num: 34,
        }));
        REGISTERED_NET_IRQS.lock().insert(32);
        REGISTERED_NET_IRQS.lock().insert(33);
        REGISTERED_NET_IRQS.lock().insert(34);

        unregister_virtio_net_irq(33, 11);

        {
            let handles = NET_IRQ_HANDLES.lock();
            assert_eq!(handles.len(), 2);
            assert_eq!(handles[0].handle_id(), 10);
            assert_eq!(handles[1].handle_id(), 12);
        }
        assert!(!REGISTERED_NET_IRQS.lock().contains(&33));
        assert!(REGISTERED_NET_IRQS.lock().contains(&32));
        assert!(REGISTERED_NET_IRQS.lock().contains(&34));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_shared_irq_preserved_when_handles_remain() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 20,
            irq_num: 40,
        }));
        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 21,
            irq_num: 40,
        }));
        REGISTERED_NET_IRQS.lock().insert(40);

        unregister_virtio_net_irq(40, 20);

        {
            let handles = NET_IRQ_HANDLES.lock();
            assert_eq!(handles.len(), 1);
            assert_eq!(handles[0].handle_id(), 21);
        }
        assert!(REGISTERED_NET_IRQS.lock().contains(&40));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_shared_irq_unregistered_when_all_handles_removed() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 30,
            irq_num: 50,
        }));
        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 31,
            irq_num: 50,
        }));
        REGISTERED_NET_IRQS.lock().insert(50);

        unregister_virtio_net_irq(50, 30);
        assert!(REGISTERED_NET_IRQS.lock().contains(&50));

        unregister_virtio_net_irq(50, 31);
        assert!(!REGISTERED_NET_IRQS.lock().contains(&50));
        assert!(NET_IRQ_HANDLES.lock().is_empty());

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_unregister_nonexistent_handle_is_noop() {
        reset_irq_state();

        NET_IRQ_HANDLES.lock().push(Arc::new(MockIrqHandle {
            id: 40,
            irq_num: 60,
        }));
        REGISTERED_NET_IRQS.lock().insert(60);

        unregister_virtio_net_irq(60, 9999);

        assert_eq!(NET_IRQ_HANDLES.lock().len(), 1);
        assert!(REGISTERED_NET_IRQS.lock().contains(&60));

        reset_irq_state();
    }

    #[def_test(serial)]
    fn test_failed_try_new_does_not_leak_irq_handle() {
        reset_irq_state();

        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::Network;
        let _dev = VirtIoNetDev::<MockHal, MockTransport, 32>::try_new(transport, None);

        assert!(NET_IRQ_HANDLES.lock().is_empty());

        reset_irq_state();
    }

    #[def_test]
    fn test_virtio_net_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIoNetDev<MockHal, MockTransport, 32>>();
    }
}
