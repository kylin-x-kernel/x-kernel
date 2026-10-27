//! Driver for VirtIO random number generator devices.
use super::common::Feature;
use crate::{
    Error, Hal, Result,
    queue::VirtQueue,
    transport::{InterruptStatus, Transport},
};

// VirtioRNG only uses one queue
const QUEUE_IDX: u16 = 0;
const QUEUE_SIZE: usize = 8;
const SUPPORTED_FEATURES: Feature = Feature::RING_INDIRECT_DESC
    .union(Feature::RING_EVENT_IDX)
    .union(Feature::VERSION_1)
    .union(Feature::ACCESS_PLATFORM);

/// Driver for a VirtIO random number generator device.
pub struct VirtIORng<H: Hal, T: Transport> {
    transport: T,
    queue: VirtQueue<H, QUEUE_SIZE>,
}

impl<H: Hal, T: Transport> VirtIORng<H, T> {
    /// Create a new driver with the given transport.
    pub fn new(mut transport: T) -> Result<Self> {
        let feat = transport.begin_init(SUPPORTED_FEATURES);
        let queue = VirtQueue::new(
            &mut transport,
            QUEUE_IDX,
            feat.contains(Feature::RING_INDIRECT_DESC),
            feat.contains(Feature::RING_EVENT_IDX),
            feat.contains(Feature::ACCESS_PLATFORM),
        )?;
        transport.finish_init();
        Ok(Self { transport, queue })
    }

    /// Request random bytes from the device to be stored into `dst`.
    pub fn request_entropy(&mut self, dst: &mut [u8]) -> Result<usize> {
        let num = self
            .queue
            .add_notify_wait_pop(&[], &mut [dst], &mut self.transport)?;
        Ok(num as usize)
    }

    /// Submit a buffer and return its token without waiting for device completion.
    ///
    /// Returns [`Error::InvalidParam`] for an empty buffer and propagates queue
    /// submission errors. A successful submission does not make the data readable.
    ///
    /// # Safety
    ///
    /// Keep `dst` allocated at the same address and length, without CPU access,
    /// until [`Self::complete_request_entropy`] successfully reclaims the request.
    /// Do not drop this driver or release the buffer while the device can access
    /// it. Cancelling a caller's wait does not discharge these obligations.
    pub unsafe fn request_entropy_nb(&mut self, dst: &mut [u8]) -> Result<u16> {
        if dst.is_empty() {
            return Err(Error::InvalidParam);
        }
        // SAFETY: the caller retains exclusive device access to this same buffer
        // until the returned token is reclaimed with complete_request_entropy.
        let token = unsafe { self.queue.add(&[], &mut [dst]) }?;
        if self.queue.should_notify() {
            self.transport.notify(QUEUE_IDX);
        }
        Ok(token)
    }

    /// Reclaim a submitted request and return the device-reported byte count.
    ///
    /// This does not wait. [`Error::NotReady`] and [`Error::WrongToken`] leave the
    /// request outstanding; retain its buffer for a later completion attempt.
    /// On success, DMA has been unshared and CPU access may resume. The caller
    /// must validate the reported length against the submitted buffer capacity.
    /// Execution-context requirements include those of [`Hal::unshare`].
    ///
    /// # Safety
    ///
    /// `token` and `dst` must identify the same outstanding submission made by
    /// [`Self::request_entropy_nb`], with the original address and length. Do not
    /// reclaim a successfully completed token again or concurrently access `dst`.
    pub unsafe fn complete_request_entropy(&mut self, token: u16, dst: &mut [u8]) -> Result<usize> {
        // SAFETY: the caller provides the original output buffer for this live
        // token; pop_used only unshares it after observing a matching completion.
        // unsafe { self.queue.pop_used(token, &[], &mut [dst]) }.map(|len| len as usize)
        let num = unsafe { self.queue.pop_used(token, &[], &mut [dst]) }?;
        Ok(num as usize)
    }

    /// Enable interrupts.
    pub fn enable_interrupts(&mut self) {
        self.queue.set_dev_notify(true);
    }

    /// Disable interrupts.
    pub fn disable_interrupts(&mut self) {
        self.queue.set_dev_notify(false);
    }

    /// Acknowledge interrupt.
    pub fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }
}

impl<H: Hal, T: Transport> Drop for VirtIORng<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(QUEUE_IDX);
    }
}
