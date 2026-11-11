use super::VirtQueue;
use crate::{Error, Hal, Result, transport::Transport};
use alloc::boxed::Box;
use core::convert::TryInto;
use core::ptr::NonNull;
use zerocopy::FromZeros;

/// A wrapper around [`VirtQueue`] that owns all the buffers that are passed to the queue.
///
/// Creating one with [`Self::empty`] publishes nothing. [`Self::fill`] is the
/// unsafe publication point: after it has published any buffer, the device must
/// be stopped before this value is dropped. Its `Drop` implementation relies on
/// that obligation to unshare outstanding buffers safely.
#[derive(Debug)]
pub struct OwningQueue<H: Hal, const SIZE: usize, const BUFFER_SIZE: usize> {
    queue: VirtQueue<H, SIZE>,
    /// Buffers indexed by the token their descriptor chain has.
    buffers: [Option<NonNull<[u8; BUFFER_SIZE]>>; SIZE],
}

impl<H: Hal, const SIZE: usize, const BUFFER_SIZE: usize> OwningQueue<H, SIZE, BUFFER_SIZE> {
    /// Constructs an `OwningQueue` without allocating or publishing buffers.
    ///
    /// Use [`Self::fill`] to allocate and publish them.
    pub fn empty(queue: VirtQueue<H, SIZE>) -> Self {
        Self {
            queue,
            buffers: [None; SIZE],
        }
    }

    /// Allocates and publishes one buffer per free slot.
    ///
    /// The caller is responsible for notifying the device if `should_notify`
    /// returns true.
    ///
    /// # Safety
    ///
    /// Once this returns `Ok`, and once it returns `Err` after any buffer has
    /// been published, this queue owns live device-visible mappings. The caller
    /// must keep it alive and must stop the device (reset the matching
    /// transport) before dropping it. A partial failure is not rolled back
    /// here: earlier published buffers remain in the queue for the device owner
    /// to release after the stop.
    pub unsafe fn fill(&mut self) -> Result<()> {
        for index in 0..SIZE {
            if self.buffers[index].is_some() {
                continue;
            }
            let mut buffer: Box<[u8; BUFFER_SIZE]> = FromZeros::new_box_zeroed().unwrap();
            // SAFETY: On success the buffer lives in `self.buffers` until this
            // queue is destroyed; the caller stops the device before that.
            let token = match unsafe { self.queue.add(&[], &mut [buffer.as_mut_slice()]) } {
                Ok(token) => token,
                Err(error) => {
                    // This buffer was never published, so it has no mapping to
                    // release and must not be recorded.
                    return Err(error);
                }
            };
            // If a buffer gets a different token then the token-to-buffer
            // mapping used by `pop` and `detach` would be wrong.
            assert_eq!(index, usize::from(token));
            self.buffers[index] = Some(NonNull::from(Box::leak(buffer)));
        }
        Ok(())
    }

    /// Returns whether the driver should notify the device after adding a new buffer to the
    /// virtqueue.
    ///
    /// This will be false if the device has suppressed notifications.
    pub fn should_notify(&self) -> bool {
        self.queue.should_notify()
    }

    /// Tells the device whether to send used buffer notifications.
    pub fn set_dev_notify(&mut self, enable: bool) {
        self.queue.set_dev_notify(enable);
    }

    /// Adds the buffer at the given index in `buffers` back to the queue.
    ///
    /// Automatically notifies the device if required.
    ///
    /// # Safety
    ///
    /// The buffer must not currently be in the RX queue, and no other references to it must exist
    /// between when this method is called and when it is popped from the queue.
    unsafe fn add_buffer_to_queue(&mut self, index: u16, transport: &mut impl Transport) -> Result {
        // SAFETY: The buffer lives as long as the queue, and the caller guarantees that it's not
        // currently in the queue or referred to anywhere else until it is popped.
        unsafe {
            let buffer = self
                .buffers
                .get_mut(usize::from(index))
                .ok_or(Error::WrongToken)?
                .as_mut()
                .ok_or(Error::WrongToken)?
                .as_mut();
            let new_token = self.queue.add(&[], &mut [buffer])?;
            // If the RX buffer somehow gets assigned a different token, then our safety assumptions
            // are broken and we can't safely continue to do anything with the device.
            assert_eq!(new_token, index);
        }

        if self.queue.should_notify() {
            transport.notify(self.queue.queue_idx);
        }

        Ok(())
    }

    fn pop(&mut self) -> Result<Option<(&[u8], u16)>> {
        let Some(token) = self.queue.peek_used() else {
            return Ok(None);
        };

        // SAFETY: The device has told us it has finished using the buffer, and there are no other
        // references to it.
        let buffer = unsafe {
            self.buffers
                .get_mut(usize::from(token))
                .ok_or(Error::WrongToken)?
                .as_mut()
                .ok_or(Error::WrongToken)?
                .as_mut()
        };
        // SAFETY: We maintain a consistent mapping of tokens to buffers, so we pass the same buffer
        // to `pop_used` as we previously passed to `add` for the token. Once we add the buffer back
        // to the RX queue then we don't access it again until next time it is popped.
        let len = unsafe { self.queue.pop_used(token, &[], &mut [buffer])? }
            .try_into()
            .unwrap();

        // The device reports how many bytes it wrote; reject if it claims more than the buffer
        // size.
        if len > BUFFER_SIZE {
            return Err(Error::IoError);
        }

        Ok(Some((&buffer[0..len], token)))
    }

    /// Checks whether there are any buffers which the device has marked as used so the driver
    /// should now process. If so, passes the first one to the `handle` function and then adds it
    /// back to the queue.
    ///
    /// Returns an error if there is an error accessing the queue or `handler` returns an error.
    /// Returns `Ok(None)` if there are no pending buffers to handle, or if `handler` returns
    /// `Ok(None)`.
    ///
    /// If `handler` panics then the buffer will not be added back to the queue, so this should be
    /// avoided.
    pub fn poll<T>(
        &mut self,
        transport: &mut impl Transport,
        handler: impl FnOnce(&[u8]) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let Some((buffer, token)) = self.pop()? else {
            return Ok(None);
        };

        let result = handler(buffer);

        // SAFETY: The buffer was just popped from the queue so it's not in it, and there won't be
        // any other references until next time it's popped.
        unsafe {
            self.add_buffer_to_queue(token, transport)?;
        }

        result
    }

    /// Unshares every outstanding buffer and frees every buffer owned by this
    /// queue.
    ///
    /// Buffers which were not in the queue (for example one whose re-add failed)
    /// are freed without an unshare. Calling this more than once, or letting
    /// `Drop` run afterwards, is harmless.
    ///
    /// # Safety
    ///
    /// The device must have stopped accessing this queue and its buffers, and
    /// this queue must not be used again afterwards.
    pub unsafe fn detach_all_after_stop(&mut self) {
        // SAFETY: The caller stopped the device; every recorded token belongs to
        // this queue and every buffer is still valid and exclusively held.
        unsafe { self.detach_and_free_all() };
    }

    /// # Safety
    ///
    /// The device must not access this queue or its buffers during or after
    /// this call.
    unsafe fn detach_and_free_all(&mut self) {
        for index in 0..SIZE {
            let Some(mut ptr) = self.buffers[index] else {
                continue;
            };
            // SAFETY: The slot owns this allocation and no reference to it
            // exists elsewhere.
            let buffer: &mut [u8] = unsafe { ptr.as_mut() };
            // SAFETY: A recorded token which is no longer outstanding (already
            // recycled or never published) returns `false` without touching the
            // buffer. The caller guarantees device access has stopped.
            unsafe {
                self.queue
                    .detach_unused_buffer(index as u16, &[], &mut [buffer]);
            }
            self.buffers[index] = None;
            // SAFETY: The pointer came from `Box::leak` and the slot is being
            // cleared, so this is the only free.
            unsafe { drop(Box::from_raw(ptr.as_ptr())) };
        }
    }
}

// SAFETY: The `buffers` can be accessed from any thread.
unsafe impl<H: Hal, const SIZE: usize, const BUFFER_SIZE: usize> Send
    for OwningQueue<H, SIZE, BUFFER_SIZE>
where
    VirtQueue<H, SIZE>: Send,
{
}

// SAFETY: An `&OwningQueue` only allows calling `should_notify`.
unsafe impl<H: Hal, const SIZE: usize, const BUFFER_SIZE: usize> Sync
    for OwningQueue<H, SIZE, BUFFER_SIZE>
where
    VirtQueue<H, SIZE>: Sync,
{
}

impl<H: Hal, const SIZE: usize, const BUFFER_SIZE: usize> Drop
    for OwningQueue<H, SIZE, BUFFER_SIZE>
{
    fn drop(&mut self) {
        // SAFETY: `fill` is unsafe and requires the owner to stop the device
        // before dropping. A queue which was never filled has nothing to
        // unshare and this only frees (or does nothing).
        unsafe { self.detach_and_free_all() };
    }
}
