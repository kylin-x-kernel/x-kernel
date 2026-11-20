//! Support for virt queues, the main mechanism for data transport on VirtIO devices.
//!
//! Types from this module are used to implement VirtIO device drivers. If you just want to use the
//! drivers provided (rather than implementing drivers for other devices) then you shouldn't need to
//! use anything from this module.

#[cfg(feature = "alloc")]
mod owning;

#[cfg(feature = "alloc")]
pub use self::owning::OwningQueue;
use crate::hal::{BufferDirection, Dma, DmaBackend, Hal, PhysAddr, StaticHal};
use crate::transport::Transport;
use crate::{Error, PAGE_SIZE, Result, align_up, pages};
#[cfg(feature = "alloc")]
use alloc::boxed::Box;
use bitflags::bitflags;
#[cfg(test)]
use core::cmp::min;
use core::convert::TryInto;
use core::hint::spin_loop;
use core::mem::{size_of, take};
#[cfg(test)]
use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU16, Ordering, fence};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

/// The outcome of [`VirtQueue::add_notify_wait_pop_checked`].
///
/// If `token` is `Some`, the buffers passed to that call are still published to
/// the device and remain owned by the queue. The caller must keep them valid,
/// stop the device, and detach them by token before releasing the buffers.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SyncError {
    /// The error which occurred.
    pub error: Error,
    /// The published token, if publication had already happened.
    pub token: Option<u16>,
}

/// The mechanism for bulk data transport on virtio devices.
///
/// Each device can have zero or more virtqueues.
///
/// * `SIZE`: The size of the queue. This is both the number of descriptors, and the number of slots
///   in the available and used rings. It must be a power of 2 and fit in a [`u16`].
#[derive(Debug)]
pub struct VirtQueue<H: Hal, const SIZE: usize, B: DmaBackend = StaticHal<H>> {
    /// Per-device DMA backend retained for allocation and mapping release.
    backend: B,
    /// DMA guard
    layout: VirtQueueLayout<H, B>,
    /// Descriptor table
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. Use `desc_shadow` instead to keep track of what we wrote to
    /// it.
    desc: NonNull<[Descriptor]>,
    /// Available ring
    ///
    /// The device may be able to modify this, even though it's not supposed to, so we shouldn't
    /// trust values read back from it. The only field we need to read currently is `idx`, so we
    /// have `avail_idx` below to use instead.
    avail: NonNull<AvailRing<SIZE>>,
    /// Used ring
    used: NonNull<UsedRing<SIZE>>,

    /// The index of queue
    queue_idx: u16,
    /// The number of descriptors currently in use.
    num_used: u16,
    /// The head desc index of the free list.
    free_head: u16,
    /// Per-head in-flight state. This is the single authority for whether a
    /// head's buffers are still owned by the device; `num_used` only counts
    /// descriptors and cannot answer that.
    in_use: [bool; SIZE],
    /// Our trusted copy of `desc` that the device can't access.
    desc_shadow: [Descriptor; SIZE],
    /// Our trusted copy of `avail.idx`.
    avail_idx: u16,
    last_used_idx: u16,
    /// Whether the `VIRTIO_F_EVENT_IDX` feature has been negotiated.
    event_idx: bool,
    /// Whether the `VIRTIO_F_ACCESS_PLATFORM` feature has been negotiated.
    access_platform: bool,
    #[cfg(feature = "alloc")]
    indirect: bool,
    #[cfg(feature = "alloc")]
    indirect_lists: [Option<NonNull<[Descriptor]>>; SIZE],
}

impl<H: Hal, const SIZE: usize> VirtQueue<H, SIZE> {
    /// Creates a new VirtQueue using the static HAL backend.
    ///
    /// * `indirect`: Whether to use indirect descriptors. This should be set if the
    ///   `VIRTIO_F_INDIRECT_DESC` feature has been negotiated with the device.
    /// * `event_idx`: Whether to use the `used_event` and `avail_event` fields for notification
    ///   suppression. This should be set if the `VIRTIO_F_EVENT_IDX` feature has been negotiated
    ///   with the device.
    pub fn new<T: Transport>(
        transport: &mut T,
        idx: u16,
        indirect: bool,
        event_idx: bool,
        access_platform: bool,
    ) -> Result<Self> {
        Self::new_with_dma(
            transport,
            idx,
            indirect,
            event_idx,
            access_platform,
            StaticHal::default(),
        )
    }
}

impl<H: Hal, const SIZE: usize, B: DmaBackend> VirtQueue<H, SIZE, B> {
    const SIZE_OK: () = assert!(SIZE.is_power_of_two() && SIZE <= u16::MAX as usize);

    /// Creates a queue whose rings and buffer mappings use the supplied device backend.
    ///
    /// Backend clones must retain the same device address space. Allocation
    /// failures release already acquired rings before returning without
    /// configuring the transport.
    pub fn new_with_dma<T: Transport>(
        transport: &mut T,
        idx: u16,
        indirect: bool,
        event_idx: bool,
        access_platform: bool,
        backend: B,
    ) -> Result<Self> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::SIZE_OK;
        // Without `alloc` there are no indirect descriptor lists to enable.
        #[cfg(not(feature = "alloc"))]
        let _ = indirect;

        if transport.queue_used(idx) {
            return Err(Error::AlreadyUsed);
        }
        if transport.max_queue_size(idx) < SIZE as u32 {
            return Err(Error::InvalidParam);
        }
        let size = SIZE as u16;

        let layout = if transport.requires_legacy_layout() {
            VirtQueueLayout::allocate_legacy(size, access_platform, backend.clone())?
        } else {
            VirtQueueLayout::allocate_flexible(size, access_platform, backend.clone())?
        };

        transport.queue_set(
            idx,
            size.into(),
            layout.descriptors_paddr(),
            layout.driver_area_paddr(),
            layout.device_area_paddr(),
        );

        let desc =
            NonNull::slice_from_raw_parts(layout.descriptors_vaddr().cast::<Descriptor>(), SIZE);
        let avail = layout.avail_vaddr().cast();
        let used = layout.used_vaddr().cast();

        let mut desc_shadow: [Descriptor; SIZE] = FromZeros::new_zeroed();
        // Link descriptors together.
        for i in 0..(size - 1) {
            desc_shadow[i as usize].next = i + 1;
            // SAFETY: `desc` is properly aligned, dereferenceable, initialised,
            // and the device won't access the descriptors for the duration of this unsafe block.
            unsafe {
                (*desc.as_ptr())[i as usize].next = i + 1;
            }
        }

        #[cfg(feature = "alloc")]
        const NONE: Option<NonNull<[Descriptor]>> = None;
        Ok(VirtQueue {
            backend,
            layout,
            desc,
            avail,
            used,
            queue_idx: idx,
            num_used: 0,
            free_head: 0,
            in_use: [false; SIZE],
            desc_shadow,
            avail_idx: 0,
            last_used_idx: 0,
            event_idx,
            access_platform,
            #[cfg(feature = "alloc")]
            indirect,
            #[cfg(feature = "alloc")]
            indirect_lists: [NONE; SIZE],
        })
    }

    /// Whether every submitted chain has been reclaimed by the driver.
    pub fn is_empty(&self) -> bool {
        self.num_used == 0
    }

    /// Reinitializes drained rings after an acknowledged device reset.
    ///
    /// # Safety
    /// The transport must have completed reset and stopped all DMA to these
    /// rings. No buffer mapping or indirect chain may remain outstanding.
    /// The caller must negotiate the same ring features before activating it.
    pub unsafe fn restore_empty<T: Transport>(&mut self, transport: &mut T) -> Result<()> {
        if self.num_used != 0 || transport.queue_used(self.queue_idx) {
            return Err(Error::NotReady);
        }
        if transport.max_queue_size(self.queue_idx) < SIZE as u32 {
            return Err(Error::InvalidParam);
        }
        self.desc_shadow = FromZeros::new_zeroed();
        for index in 0..SIZE - 1 {
            self.desc_shadow[index].next = (index + 1) as u16;
        }
        // SAFETY: the caller guarantees completed reset and DMA exclusion;
        // these are our exclusively owned, correctly sized coherent ring areas.
        unsafe {
            core::ptr::copy_nonoverlapping(self.desc_shadow.as_ptr(), self.desc.as_ptr().cast::<Descriptor>(), SIZE);
        }
        // SAFETY: reset excludes DMA into the owned, initialized available ring.
        unsafe { core::ptr::write_bytes(self.avail.as_ptr(), 0, 1) };
        // SAFETY: the same exclusion covers the separately owned used ring.
        unsafe { core::ptr::write_bytes(self.used.as_ptr(), 0, 1) };
        self.free_head = 0;
        self.avail_idx = 0;
        self.last_used_idx = 0;
        // Publish descriptor/free-list and zeroed ring stores before transport
        // queue registration makes these DMA addresses visible to the device.
        fence(Ordering::Release);
        transport.queue_set(
            self.queue_idx, SIZE as u32,
            self.layout.descriptors_paddr(), self.layout.driver_area_paddr(),
            self.layout.device_area_paddr(),
        );
        Ok(())
    }

    /// Add buffers to the virtqueue, return a token.
    ///
    /// The buffers must not be empty.
    ///
    /// Ref: linux virtio_ring.c virtqueue_add
    ///
    /// # Safety
    ///
    /// The input and output buffers must remain valid and not be accessed until a call to
    /// `pop_used` with the returned token succeeds, or the device is reset and
    /// `detach_unused_buffer` reclaims that token.
    pub unsafe fn add<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        if inputs.is_empty() && outputs.is_empty() {
            return Err(Error::InvalidParam);
        }
        let descriptors_needed = inputs.len() + outputs.len();
        // Only consider indirect descriptors if the alloc feature is enabled, as they require
        // allocation.
        #[cfg(feature = "alloc")]
        if self.num_used as usize + 1 > SIZE
            || descriptors_needed > SIZE
            || (!self.indirect && self.num_used as usize + descriptors_needed > SIZE)
        {
            return Err(Error::QueueFull);
        }
        #[cfg(not(feature = "alloc"))]
        if self.num_used as usize + descriptors_needed > SIZE {
            return Err(Error::QueueFull);
        }

        if InputOutputIter::new(inputs, outputs)
            .any(|(buffer, _)| buffer.is_empty() || buffer.len() > u32::MAX as usize)
        {
            return Err(Error::InvalidParam);
        }

        #[cfg(feature = "alloc")]
        let head = if self.indirect && descriptors_needed > 1 {
            self.add_indirect(inputs, outputs)?
        } else {
            self.add_direct(inputs, outputs)?
        };
        #[cfg(not(feature = "alloc"))]
        let head = self.add_direct(inputs, outputs)?;

        // The head stays in flight until `recycle_descriptors` unshares it; a
        // failed pop in particular must not make these buffers look free.
        self.in_use[usize::from(head)] = true;

        let avail_slot = self.avail_idx & (SIZE as u16 - 1);
        // SAFETY: `self.avail` is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr()).ring[avail_slot as usize] = head;
        }

        // Write barrier so that device sees changes to descriptor table and available ring before
        // change to available index.
        fence(Ordering::SeqCst);

        // increase head of avail ring
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: `self.avail` is properly aligned, dereferenceable and initialised.
        unsafe {
            (*self.avail.as_ptr())
                .idx
                .store(self.avail_idx, Ordering::Release);
        }

        // Full barrier so that the device sees our update to `avail.idx` before we read
        // `avail_event` or the used ring flags.
        fence(Ordering::SeqCst);

        Ok(head)
    }

    fn add_direct<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        // allocate descriptors from free list
        let head = self.free_head;
        let mut next = head;
        let mut last = head;

        for (mapped, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
            // Write to desc_shadow then copy.
            let desc = &mut self.desc_shadow[usize::from(next)];
            // SAFETY: Our caller promises that the buffers live at least until `pop_used`
            // returns them, and none are published before every mapping succeeds.
            let result = unsafe {
                desc.set_buf(
                    &self.backend,
                    buffer,
                    direction,
                    DescFlags::NEXT,
                    self.access_platform,
                )
            };
            if let Err(error) = result {
                let mut rollback = head;
                for (buffer, direction) in InputOutputIter::new(inputs, outputs).take(mapped) {
                    let desc = &mut self.desc_shadow[usize::from(rollback)];
                    // SAFETY: This successful mapping has not been published to the device.
                    unsafe {
                        self.backend
                            .unshare(desc.addr, buffer, direction, self.access_platform);
                    }
                    desc.unset_buf();
                    rollback = desc.next;
                }
                return Err(error);
            }
            last = next;
            next = desc.next;

            self.write_desc(last);
        }

        // set last_elem.next = NULL
        self.desc_shadow[usize::from(last)]
            .flags
            .remove(DescFlags::NEXT);
        self.write_desc(last);

        self.free_head = next;
        self.num_used += (inputs.len() + outputs.len()) as u16;

        Ok(head)
    }

    #[cfg(feature = "alloc")]
    fn add_indirect<'a, 'b>(
        &mut self,
        inputs: &'a [&'b [u8]],
        outputs: &'a mut [&'b mut [u8]],
    ) -> Result<u16> {
        let head = self.free_head;

        // Allocate and fill in indirect descriptor list.
        let mut indirect_list =
            <[Descriptor]>::new_box_zeroed_with_elems(inputs.len() + outputs.len())
                .map_err(|_| Error::DmaError)?;
        let mut mapped = 0;
        let result = (|| {
            for (index, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
                let desc = &mut indirect_list[index];
                // SAFETY: Our caller promises that the buffers live at least until `pop_used`
                // returns them, and none are published before every mapping succeeds.
                unsafe {
                    desc.set_buf(
                        &self.backend,
                        buffer,
                        direction,
                        DescFlags::NEXT,
                        self.access_platform,
                    )?;
                }
                mapped += 1;
                desc.next = (index + 1) as u16;
            }
            indirect_list
                .last_mut()
                .unwrap()
                .flags
                .remove(DescFlags::NEXT);
            let direct_desc = &mut self.desc_shadow[usize::from(head)];
            // SAFETY: The box remains owned until mapping succeeds, then is retained
            // in `indirect_lists` until completion. It has not been published yet.
            unsafe {
                direct_desc.set_buf(
                    &self.backend,
                    indirect_list.as_mut_bytes().into(),
                    BufferDirection::DriverToDevice,
                    DescFlags::INDIRECT,
                    self.access_platform,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            for (index, (buffer, direction)) in InputOutputIter::new(inputs, outputs)
                .take(mapped)
                .enumerate()
            {
                // SAFETY: Only successful mappings are released, before any device publication.
                unsafe {
                    self.backend.unshare(
                        indirect_list[index].addr,
                        buffer,
                        direction,
                        self.access_platform,
                    );
                }
            }
            return Err(error);
        }

        // Need to store pointer to indirect_list too, because `set_buf` will only store
        // the physical DMA address which might be different.
        assert!(self.indirect_lists[usize::from(head)].is_none());
        // `Box::leak` prevents the indirect list from being freed when this
        // function returns; `recycle_descriptors` frees it after the buffer chain
        // is popped.
        self.indirect_lists[usize::from(head)] = Some(NonNull::from(Box::leak(indirect_list)));
        self.free_head = self.desc_shadow[usize::from(head)].next;
        self.write_desc(head);
        self.num_used += 1;

        Ok(head)
    }

    /// Add the given buffers to the virtqueue, notifies the device, blocks until the device uses
    /// them, then pops them.
    ///
    /// This assumes that the device isn't processing any other buffers at the same time.
    ///
    /// The buffers must not be empty.
    pub fn add_notify_wait_pop<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
        transport: &mut impl Transport,
    ) -> Result<u32> {
        // SAFETY: We don't return until the same token has been popped, so the buffers remain
        // valid and are not otherwise accessed until then.
        let token = unsafe { self.add(inputs, outputs) }?;

        // Notify the queue.
        if self.should_notify() {
            transport.notify(self.queue_idx);
        }

        // Wait until there is at least one element in the used ring.
        while !self.can_pop() {
            spin_loop();
        }

        // SAFETY: These are the same buffers as we passed to `add` above and they are still valid.
        unsafe { self.pop_used(token, inputs, outputs) }
    }

    /// Add buffers, notify the device, wait for them to be used, and pop them,
    /// reporting whether publication happened if an error occurs.
    ///
    /// This assumes that the device isn't processing any other buffers at the
    /// same time. It busy-waits until an element appears in the used ring; it
    /// has no timeout, and the caller remains responsible for stopping a device
    /// which does not respond.
    ///
    /// # Safety
    ///
    /// The buffers must stay valid and must not be otherwise accessed until one
    /// of these is true:
    ///
    /// - `Ok` is returned; the queue has unshared them.
    /// - `Err` with `token: Some` is returned, and the caller has stopped the
    ///   device and detached that token with [`Self::detach_unused_buffer`].
    /// - `Err` with `token: None` is returned; nothing was published, so the
    ///   buffers may be released immediately.
    pub unsafe fn add_notify_wait_pop_checked<'a>(
        &mut self,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
        transport: &mut impl Transport,
    ) -> core::result::Result<u32, SyncError> {
        // SAFETY: The caller keeps the buffers valid for the whole call and,
        // after a published error, until the detach obligation is met.
        let token = match unsafe { self.add(inputs, outputs) } {
            Ok(token) => token,
            Err(error) => return Err(SyncError { error, token: None }),
        };

        if self.should_notify() {
            transport.notify(self.queue_idx);
        }

        while !self.can_pop() {
            spin_loop();
        }

        // SAFETY: These are the same buffers as passed to `add` above and they
        // are still valid; on failure the returned token records their state.
        match unsafe { self.pop_used(token, inputs, outputs) } {
            Ok(len) => Ok(len),
            Err(error) => Err(SyncError {
                error,
                token: Some(token),
            }),
        }
    }

    /// Advise the device whether used buffer notifications are needed.
    ///
    /// See Virtio v1.1 2.6.7 Used Buffer Notification Suppression
    pub fn set_dev_notify(&mut self, enable: bool) {
        let avail_ring_flags = if enable { 0x0000 } else { 0x0001 };
        if !self.event_idx {
            // SAFETY: `self.avail` points to a valid, aligned, initialised, dereferenceable, readable
            // instance of `AvailRing`.
            unsafe {
                (*self.avail.as_ptr())
                    .flags
                    .store(avail_ring_flags, Ordering::Release)
            }
        }
    }

    /// Returns whether the driver should notify the device after adding a new buffer to the
    /// virtqueue.
    ///
    /// This will be false if the device has suppressed notifications.
    pub fn should_notify(&self) -> bool {
        if self.event_idx {
            // SAFETY: `self.used` points to a valid, aligned, initialised, dereferenceable, readable
            // instance of `UsedRing`.
            let avail_event = unsafe { (*self.used.as_ptr()).avail_event.load(Ordering::Acquire) };
            self.avail_idx >= avail_event.wrapping_add(1)
        } else {
            // SAFETY: `self.used` points to a valid, aligned, initialised, dereferenceable, readable
            // instance of `UsedRing`.
            unsafe { (*self.used.as_ptr()).flags.load(Ordering::Acquire) & 0x0001 == 0 }
        }
    }

    /// Copies the descriptor at the given index from `desc_shadow` to `desc`, so it can be seen by
    /// the device.
    fn write_desc(&mut self, index: u16) {
        let index = usize::from(index);
        // SAFETY: `self.desc` is properly aligned, dereferenceable and initialised, and nothing
        // else reads or writes the descriptor during this block.
        unsafe {
            (*self.desc.as_ptr())[index] = self.desc_shadow[index].clone();
        }
    }

    /// Returns whether there is a used element that can be popped.
    pub fn can_pop(&self) -> bool {
        // SAFETY: `self.used` points to a valid, aligned, initialised, dereferenceable, readable
        // instance of `UsedRing`.
        self.last_used_idx != unsafe { (*self.used.as_ptr()).idx.load(Ordering::Acquire) }
    }

    /// Returns the descriptor index (a.k.a. token) of the next used element without popping it, or
    /// `None` if the used ring is empty.
    pub fn peek_used(&self) -> Option<u16> {
        if self.can_pop() {
            let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
            // SAFETY: `self.used` points to a valid, aligned, initialised, dereferenceable,
            // readable instance of `UsedRing`.
            Some(unsafe { (*self.used.as_ptr()).ring[last_used_slot as usize].id as u16 })
        } else {
            None
        }
    }

    /// Returns the number of free descriptors.
    pub fn available_desc(&self) -> usize {
        #[cfg(feature = "alloc")]
        if self.indirect {
            return if usize::from(self.num_used) == SIZE {
                0
            } else {
                SIZE
            };
        }

        SIZE - usize::from(self.num_used)
    }

    /// Unshares buffers in the list starting at descriptor index `head` and adds them to the free
    /// list. Unsharing may involve copying data back to the original buffers, so they must be
    /// passed in too.
    ///
    /// This will push all linked descriptors at the front of the free list.
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add`.
    unsafe fn recycle_descriptors<'a>(
        &mut self,
        head: u16,
        inputs: &[&'a [u8]],
        outputs: &mut [&'a mut [u8]],
    ) {
        let original_free_head = self.free_head;
        self.free_head = head;

        let head_desc = &mut self.desc_shadow[usize::from(head)];
        if head_desc.flags.contains(DescFlags::INDIRECT) {
            #[cfg(feature = "alloc")]
            {
                // Find the indirect descriptor list, unshare it and move its descriptor to the free
                // list.
                let indirect_list = self.indirect_lists[usize::from(head)].take().unwrap();
                // SAFETY: We allocated the indirect list in `add_indirect`, and the device has
                // finished accessing it by this point.
                let mut indirect_list = unsafe { Box::from_raw(indirect_list.as_ptr()) };
                let paddr = head_desc.addr;
                head_desc.unset_buf();
                self.num_used -= 1;
                head_desc.next = original_free_head;

                // SAFETY: `paddr` comes from this backend's previous `share` (inside
                // `Descriptor::set_buf`, which was called from `add_direct` or `add_indirect`).
                // `indirect_list` is owned by this function and is not accessed from any other threads.
                unsafe {
                    self.backend.unshare(
                        paddr,
                        indirect_list.as_mut_bytes().into(),
                        BufferDirection::DriverToDevice,
                        self.access_platform,
                    );
                }

                // Unshare the buffers in the indirect descriptor list, and free it.
                assert_eq!(indirect_list.len(), inputs.len() + outputs.len());
                for (i, (buffer, direction)) in InputOutputIter::new(inputs, outputs).enumerate() {
                    assert_ne!(buffer.len(), 0);

                    // SAFETY: The caller ensures that the buffer is valid and matches the
                    // descriptor from which we got `paddr`.
                    unsafe {
                        // Unshare the buffer (and perhaps copy its contents back to the original
                        // buffer).
                        self.backend.unshare(
                            indirect_list[i].addr,
                            buffer,
                            direction,
                            self.access_platform,
                        );
                    }
                }
                drop(indirect_list);
            }
        } else {
            let mut next = Some(head);

            for (buffer, direction) in InputOutputIter::new(inputs, outputs) {
                assert_ne!(buffer.len(), 0);

                let desc_index = next.expect("Descriptor chain was shorter than expected.");
                let desc = &mut self.desc_shadow[usize::from(desc_index)];

                let paddr = desc.addr;
                desc.unset_buf();
                self.num_used -= 1;
                next = desc.next();
                if next.is_none() {
                    desc.next = original_free_head;
                }

                self.write_desc(desc_index);

                // SAFETY: The caller ensures that the buffer is valid and matches the descriptor
                // from which we got `paddr`.
                unsafe {
                    // Unshare the buffer (and perhaps copy its contents back to the original buffer).
                    self.backend
                        .unshare(paddr, buffer, direction, self.access_platform);
                }
            }

            if next.is_some() {
                panic!("Descriptor chain was longer than expected.");
            }
        }

        self.in_use[usize::from(head)] = false;
    }

    /// Returns the number of heads still owned by the device.
    ///
    /// This counts publication, not descriptor use: a head whose pop failed
    /// with [`Error::WrongToken`] is still outstanding.
    pub fn outstanding_heads(&self) -> usize {
        self.in_use.iter().filter(|&&in_use| in_use).count()
    }

    /// Unshares and frees the buffers of one published head which the device
    /// will not complete on its own.
    ///
    /// Returns `false` without touching anything if `token` is not an
    /// outstanding head.
    ///
    /// # Safety
    ///
    /// The device must have stopped accessing this queue and its buffers, and
    /// the caller must not use this queue again afterwards. The buffers in
    /// `inputs` and `outputs` must be the ones passed to the `add` call which
    /// returned `token`, and must still be valid and exclusively held.
    pub unsafe fn detach_unused_buffer<'a>(
        &mut self,
        token: u16,
        inputs: &'a [&'a [u8]],
        outputs: &'a mut [&'a mut [u8]],
    ) -> bool {
        let index = usize::from(token);
        if index >= SIZE || !self.in_use[index] {
            return false;
        }
        // SAFETY: The caller stopped device access and supplied the matching buffers.
        unsafe {
            self.recycle_descriptors(token, inputs, outputs);
        }
        true
    }

    /// If the given token is next on the device used queue, pops it and returns the total buffer
    /// length which was used (written) by the device.
    ///
    /// Ref: linux virtio_ring.c virtqueue_get_buf_ctx
    ///
    /// # Safety
    ///
    /// The buffers in `inputs` and `outputs` must match the set of buffers originally added to the
    /// queue by `add` when it returned the token being passed in here.
    pub unsafe fn pop_used<'a>(
        &mut self,
        token: u16,
        inputs: &[&'a [u8]],
        outputs: &mut [&'a mut [u8]],
    ) -> Result<u32> {
        if !self.can_pop() {
            return Err(Error::NotReady);
        }

        // Get the index of the start of the descriptor chain for the next element in the used ring.
        let last_used_slot = self.last_used_idx & (SIZE as u16 - 1);
        let index;
        let len;
        // SAFETY: `self.used` points to a valid, aligned, initialised, dereferenceable, readable
        // instance of `UsedRing`.
        unsafe {
            index = (*self.used.as_ptr()).ring[last_used_slot as usize].id;
            len = (*self.used.as_ptr()).ring[last_used_slot as usize].len;
        }

        if index != u32::from(token) {
            // The device used a different descriptor chain to the one we were expecting.
            return Err(Error::WrongToken);
        }

        // SAFETY: The caller ensures the buffers are valid and match the descriptor.
        unsafe {
            self.recycle_descriptors(token, inputs, outputs);
        }
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        if self.event_idx {
            // SAFETY: `self.avail` points to a valid, aligned, initialised, dereferenceable,
            // readable instance of `AvailRing`.
            unsafe {
                (*self.avail.as_ptr())
                    .used_event
                    .store(self.last_used_idx, Ordering::Release);
            }
        }

        Ok(len)
    }
}

// SAFETY: Ring memory and pointers may move between threads; the retained backend is Send.
unsafe impl<H: Hal, const SIZE: usize, B: DmaBackend + Send> Send for VirtQueue<H, SIZE, B> {}

// SAFETY: A `&VirtQueue` only allows reading from the various pointers it contains, so there is no
// data race. The retained backend is also Sync.
unsafe impl<H: Hal, const SIZE: usize, B: DmaBackend + Sync> Sync for VirtQueue<H, SIZE, B> {}

/// The inner layout of a VirtQueue.
///
/// Ref: 2.6 Split Virtqueues
#[derive(Debug)]
enum VirtQueueLayout<H: Hal, B: DmaBackend> {
    Legacy {
        dma: Dma<H, B>,
        avail_offset: usize,
        used_offset: usize,
    },
    Modern {
        /// The region used for the descriptor area and driver area.
        driver_to_device_dma: Dma<H, B>,
        /// The region used for the device area.
        device_to_driver_dma: Dma<H, B>,
        /// The offset from the start of the `driver_to_device_dma` region to the driver area
        /// (available ring).
        avail_offset: usize,
    },
}

impl<H: Hal, B: DmaBackend> VirtQueueLayout<H, B> {
    /// Allocates a single DMA region containing all parts of the virtqueue, following the layout
    /// required by legacy interfaces.
    ///
    /// Ref: 2.6.2 Legacy Interfaces: A Note on Virtqueue Layout
    fn allocate_legacy(queue_size: u16, access_platform: bool, backend: B) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        let size = align_up(desc + avail) + align_up(used);
        // Allocate contiguous pages.
        let dma = Dma::new_with_dma(
            size / PAGE_SIZE,
            BufferDirection::Both,
            access_platform,
            backend,
        )?;
        Ok(Self::Legacy {
            dma,
            avail_offset: desc,
            used_offset: align_up(desc + avail),
        })
    }

    /// Allocates separate DMA regions for the the different parts of the virtqueue, as supported by
    /// non-legacy interfaces.
    ///
    /// This is preferred over `allocate_legacy` where possible as it reduces memory fragmentation
    /// and allows the HAL to know which DMA regions are used in which direction.
    fn allocate_flexible(queue_size: u16, access_platform: bool, backend: B) -> Result<Self> {
        let (desc, avail, used) = queue_part_sizes(queue_size);
        let driver_to_device_dma = Dma::new_with_dma(
            pages(desc + avail),
            BufferDirection::DriverToDevice,
            access_platform,
            backend.clone(),
        )?;
        let device_to_driver_dma = Dma::new_with_dma(
            pages(used),
            BufferDirection::DeviceToDriver,
            access_platform,
            backend,
        )?;
        Ok(Self::Modern {
            driver_to_device_dma,
            device_to_driver_dma,
            avail_offset: desc,
        })
    }

    /// Returns the physical address of the descriptor area.
    fn descriptors_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy { dma, .. } => dma.paddr(),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.paddr(),
        }
    }

    /// Returns a pointer to the descriptor table (in the descriptor area).
    fn descriptors_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy { dma, .. } => dma.vaddr(0),
            Self::Modern {
                driver_to_device_dma,
                ..
            } => driver_to_device_dma.vaddr(0),
        }
    }

    /// Returns the physical address of the driver area.
    fn driver_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.paddr() + *avail_offset as u64,
            Self::Modern {
                driver_to_device_dma,
                avail_offset,
                ..
            } => driver_to_device_dma.paddr() + *avail_offset as u64,
        }
    }

    /// Returns a pointer to the available ring (in the driver area).
    fn avail_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, avail_offset, ..
            } => dma.vaddr(*avail_offset),
            Self::Modern {
                driver_to_device_dma,
                avail_offset,
                ..
            } => driver_to_device_dma.vaddr(*avail_offset),
        }
    }

    /// Returns the physical address of the device area.
    fn device_area_paddr(&self) -> PhysAddr {
        match self {
            Self::Legacy {
                used_offset, dma, ..
            } => dma.paddr() + *used_offset as u64,
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.paddr(),
        }
    }

    /// Returns a pointer to the used ring (in the driver area).
    fn used_vaddr(&self) -> NonNull<u8> {
        match self {
            Self::Legacy {
                dma, used_offset, ..
            } => dma.vaddr(*used_offset),
            Self::Modern {
                device_to_driver_dma,
                ..
            } => device_to_driver_dma.vaddr(0),
        }
    }
}

/// Returns the size in bytes of the descriptor table, available ring and used ring for a given
/// queue size.
///
/// Ref: 2.6 Split Virtqueues
fn queue_part_sizes(queue_size: u16) -> (usize, usize, usize) {
    assert!(
        queue_size.is_power_of_two(),
        "queue size should be a power of 2"
    );
    let queue_size = usize::from(queue_size);
    let desc = size_of::<Descriptor>() * queue_size;
    let avail = size_of::<u16>() * (3 + queue_size);
    let used = size_of::<u16>() * 3 + size_of::<UsedElem>() * queue_size;
    (desc, avail, used)
}

#[repr(C, align(16))]
#[derive(Clone, Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
pub(crate) struct Descriptor {
    addr: u64,
    len: u32,
    flags: DescFlags,
    next: u16,
}

impl Descriptor {
    /// Sets the buffer address, length and flags, and shares it with the device.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the buffer lives at least as long as the descriptor is active.
    unsafe fn set_buf<B: DmaBackend>(
        &mut self,
        backend: &B,
        buf: NonNull<[u8]>,
        direction: BufferDirection,
        extra_flags: DescFlags,
        access_platform: bool,
    ) -> Result<()> {
        let len = buf.len().try_into().map_err(|_| Error::InvalidParam)?;
        // SAFETY: Our caller promises that the buffer is valid.
        let address = unsafe { backend.share(buf, direction, access_platform)? };
        self.addr = address;
        self.len = len;
        self.flags = extra_flags
            | match direction {
                BufferDirection::DeviceToDriver => DescFlags::WRITE,
                BufferDirection::DriverToDevice => DescFlags::empty(),
                BufferDirection::Both => {
                    panic!("Buffer passed to device should never use BufferDirection::Both.")
                }
            };
        Ok(())
    }

    /// Sets the buffer address and length to 0.
    ///
    /// This must only be called once the device has finished using the descriptor.
    fn unset_buf(&mut self) {
        self.addr = 0;
        self.len = 0;
    }

    /// Returns the index of the next descriptor in the chain if the `NEXT` flag is set, or `None`
    /// if it is not (and thus this descriptor is the end of the chain).
    fn next(&self) -> Option<u16> {
        if self.flags.contains(DescFlags::NEXT) {
            Some(self.next)
        } else {
            None
        }
    }
}

/// Descriptor flags
#[derive(
    Copy, Clone, Debug, Default, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq,
)]
#[repr(transparent)]
struct DescFlags(u16);

bitflags! {
    impl DescFlags: u16 {
        const NEXT = 1;
        const WRITE = 2;
        const INDIRECT = 4;
    }
}

/// The driver uses the available ring to offer buffers to the device:
/// each ring entry refers to the head of a descriptor chain.
/// It is only written by the driver and read by the device.
#[repr(C)]
#[derive(Debug)]
struct AvailRing<const SIZE: usize> {
    flags: AtomicU16,
    /// A driver MUST NOT decrement the idx.
    idx: AtomicU16,
    ring: [u16; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    used_event: AtomicU16,
}

/// The used ring is where the device returns buffers once it is done with them:
/// it is only written to by the device, and read by the driver.
#[repr(C)]
#[derive(Debug)]
struct UsedRing<const SIZE: usize> {
    flags: AtomicU16,
    idx: AtomicU16,
    ring: [UsedElem; SIZE],
    /// Only used if `VIRTIO_F_EVENT_IDX` is negotiated.
    avail_event: AtomicU16,
}

#[repr(C)]
#[derive(Debug)]
struct UsedElem {
    id: u32,
    len: u32,
}

struct InputOutputIter<'a, 'b> {
    inputs: &'a [&'b [u8]],
    outputs: &'a mut [&'b mut [u8]],
}

impl<'a, 'b> InputOutputIter<'a, 'b> {
    fn new(inputs: &'a [&'b [u8]], outputs: &'a mut [&'b mut [u8]]) -> Self {
        Self { inputs, outputs }
    }
}

impl Iterator for InputOutputIter<'_, '_> {
    type Item = (NonNull<[u8]>, BufferDirection);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(input) = take_first(&mut self.inputs) {
            Some(((*input).into(), BufferDirection::DriverToDevice))
        } else {
            let output = take_first_mut(&mut self.outputs)?;
            Some(((*output).into(), BufferDirection::DeviceToDriver))
        }
    }
}

// TODO: Use `slice::take_first` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first<'a, T>(slice: &mut &'a [T]) -> Option<&'a T> {
    let (first, rem) = slice.split_first()?;
    *slice = rem;
    Some(first)
}

// TODO: Use `slice::take_first_mut` once it is stable
// (https://github.com/rust-lang/rust/issues/62280).
fn take_first_mut<'a, T>(slice: &mut &'a mut [T]) -> Option<&'a mut T> {
    let (first, rem) = take(slice).split_first_mut()?;
    *slice = rem;
    Some(first)
}

/// Rewrites the id of the most recently published used-ring entry.
///
/// Test-only helper for simulating a device which completes a different
/// descriptor chain than the driver submitted.
#[cfg(test)]
pub(crate) fn fake_corrupt_last_used_id<const QUEUE_SIZE: usize>(
    queue_device_area: *mut u8,
    id: impl Into<u32>,
) {
    let used_ring = queue_device_area as *mut UsedRing<QUEUE_SIZE>;
    // SAFETY: The caller passes the device area of a configured fake queue and
    // at least one used entry has been published.
    unsafe {
        let used_idx = (*used_ring).idx.load(Ordering::Acquire);
        assert_ne!(used_idx, 0);
        let slot = used_idx.wrapping_sub(1) & (QUEUE_SIZE as u16 - 1);
        (*used_ring).ring[slot as usize].id = id.into();
    }
}

/// Simulates the device reading from a VirtIO queue and writing a response back, for use in tests.
///
/// The fake device always uses descriptors in order.
///
/// Returns true if a descriptor chain was available and processed, or false if no descriptors were
/// available.
#[cfg(test)]
pub(crate) fn fake_read_write_queue<const QUEUE_SIZE: usize>(
    descriptors: *const [Descriptor; QUEUE_SIZE],
    queue_driver_area: *const u8,
    queue_device_area: *mut u8,
    handler: impl FnOnce(Vec<u8>) -> Vec<u8>,
) -> bool {
    use core::{ops::Deref, slice};

    let available_ring = queue_driver_area as *const AvailRing<QUEUE_SIZE>;
    let used_ring = queue_device_area as *mut UsedRing<QUEUE_SIZE>;

    // Safe because the various pointers are properly aligned, dereferenceable, initialised, and
    // nothing else accesses them during this block.
    unsafe {
        // Make sure there is actually at least one descriptor available to read from.
        if (*available_ring).idx.load(Ordering::Acquire) == (*used_ring).idx.load(Ordering::Acquire)
        {
            return false;
        }
        // The fake device always uses descriptors in order, like VIRTIO_F_IN_ORDER, so
        // `used_ring.idx` marks the next descriptor we should take from the available ring.
        let next_slot = (*used_ring).idx.load(Ordering::Acquire) & (QUEUE_SIZE as u16 - 1);
        let head_descriptor_index = (*available_ring).ring[next_slot as usize];
        let mut descriptor = &(*descriptors)[head_descriptor_index as usize];

        let input_length;
        let output;
        if descriptor.flags.contains(DescFlags::INDIRECT) {
            // The descriptor shouldn't have any other flags if it is indirect.
            assert_eq!(descriptor.flags, DescFlags::INDIRECT);

            // Loop through all input descriptors in the indirect descriptor list, reading data from
            // them.
            let indirect_descriptor_list: &[Descriptor] = zerocopy::Ref::into_ref(
                zerocopy::Ref::<_, [Descriptor]>::from_bytes(slice::from_raw_parts(
                    descriptor.addr as *const u8,
                    descriptor.len as usize,
                ))
                .unwrap(),
            );
            let mut input = Vec::new();
            let mut indirect_descriptor_index = 0;
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                if indirect_descriptor.flags.contains(DescFlags::WRITE) {
                    break;
                }

                input.extend_from_slice(slice::from_raw_parts(
                    indirect_descriptor.addr as *const u8,
                    indirect_descriptor.len as usize,
                ));

                indirect_descriptor_index += 1;
            }
            input_length = input.len();

            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            while indirect_descriptor_index < indirect_descriptor_list.len() {
                let indirect_descriptor = &indirect_descriptor_list[indirect_descriptor_index];
                assert!(indirect_descriptor.flags.contains(DescFlags::WRITE));

                let length_to_write = min(remaining_output.len(), indirect_descriptor.len as usize);
                ptr::copy(
                    remaining_output.as_ptr(),
                    indirect_descriptor.addr as *mut u8,
                    length_to_write,
                );
                remaining_output = &remaining_output[length_to_write..];

                indirect_descriptor_index += 1;
            }
            assert_eq!(remaining_output.len(), 0);
        } else {
            // Loop through all input descriptors in the chain, reading data from them.
            let mut input = Vec::new();
            while !descriptor.flags.contains(DescFlags::WRITE) {
                input.extend_from_slice(slice::from_raw_parts(
                    descriptor.addr as *const u8,
                    descriptor.len as usize,
                ));

                if let Some(next) = descriptor.next() {
                    descriptor = &(*descriptors)[next as usize];
                } else {
                    break;
                }
            }
            input_length = input.len();

            // Let the test handle the request.
            output = handler(input);

            // Write the response to the remaining descriptors.
            let mut remaining_output = output.deref();
            if descriptor.flags.contains(DescFlags::WRITE) {
                loop {
                    assert!(descriptor.flags.contains(DescFlags::WRITE));

                    let length_to_write = min(remaining_output.len(), descriptor.len as usize);
                    ptr::copy(
                        remaining_output.as_ptr(),
                        descriptor.addr as *mut u8,
                        length_to_write,
                    );
                    remaining_output = &remaining_output[length_to_write..];

                    if let Some(next) = descriptor.next() {
                        descriptor = &(*descriptors)[next as usize];
                    } else {
                        break;
                    }
                }
            }
            assert_eq!(remaining_output.len(), 0);
        }

        // Mark the buffer as used.
        (*used_ring).ring[next_slot as usize].id = head_descriptor_index.into();
        (*used_ring).ring[next_slot as usize].len = (input_length + output.len()) as u32;
        (*used_ring).idx.fetch_add(1, Ordering::AcqRel);

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        device::common::Feature,
        hal::fake::FakeHal,
        transport::{
            DeviceType,
            fake::{FakeTransport, QueueStatus, State},
            mmio::{MODERN_VERSION, MmioTransport, VirtIOHeader},
        },
    };
    use safe_mmio::UniqueMmioPointer;
    use std::sync::{Arc, Mutex};

    #[test]
    fn drained_queue_transfers_again_after_repeated_transport_reset() {
        let state = Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], ())));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            state: state.clone(),
        };
        let mut queue = VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();
        for value in 0..3u8 {
            let input = [value];
            let inputs = [&input[..]];
            // SAFETY: input stays live and immutable until the chain is reclaimed.
            let token = unsafe { queue.add(&inputs, &mut []) }.unwrap();
            assert_eq!(state.lock().unwrap().read_from_queue::<4>(0), vec![value]);
            // SAFETY: the fake device completed this chain and buffers match add.
            unsafe { queue.pop_used(token, &inputs, &mut []) }.unwrap();
            transport.queue_unset(0);
            // SAFETY: fake reset synchronously stops DMA and all chains retired.
            unsafe { queue.restore_empty(&mut transport) }.unwrap();
        }
    }

    #[test]
    fn queue_too_big() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 8>::new(&mut transport, 0, false, false, false).unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn queue_already_used() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();
        assert_eq!(
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap_err(),
            Error::AlreadyUsed
        );
    }

    #[test]
    fn add_empty() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();
        assert_eq!(
            unsafe { queue.add(&[], &mut []) }.unwrap_err(),
            Error::InvalidParam
        );
    }

    #[test]
    fn add_too_many() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);
        assert_eq!(
            unsafe { queue.add(&[&[], &[], &[]], &mut [&mut [], &mut []]) }.unwrap_err(),
            Error::QueueFull
        );
    }

    #[test]
    fn add_buffers() {
        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 0);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let first_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(first_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[first_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let second_descriptor_index =
                (*queue.desc.as_ptr())[first_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[second_descriptor_index as usize].flags,
                DescFlags::NEXT
            );
            let third_descriptor_index =
                (*queue.desc.as_ptr())[second_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].len,
                2
            );
            assert_eq!(
                (*queue.desc.as_ptr())[third_descriptor_index as usize].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            let fourth_descriptor_index =
                (*queue.desc.as_ptr())[third_descriptor_index as usize].next;
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].len,
                1
            );
            assert_eq!(
                (*queue.desc.as_ptr())[fourth_descriptor_index as usize].flags,
                DescFlags::WRITE
            );
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn add_buffers_indirect() {
        use core::ptr::slice_from_raw_parts;

        let mut header = VirtIOHeader::make_fake_header(MODERN_VERSION, 1, 0, 0, 4);
        let mut transport = MmioTransport::new_from_unique(
            UniqueMmioPointer::from(&mut header),
            UniqueMmioPointer::from([].as_mut_slice()),
        )
        .unwrap();
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, true, false, false).unwrap();
        assert_eq!(queue.available_desc(), 4);

        // Add a buffer chain consisting of two device-readable parts followed by two
        // device-writable parts.
        let token = unsafe { queue.add(&[&[1, 2], &[3]], &mut [&mut [0, 0], &mut [0]]) }.unwrap();

        assert_eq!(queue.available_desc(), 4);
        assert!(!queue.can_pop());

        // Safe because the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            let indirect_descriptor_index = (*queue.avail.as_ptr()).ring[0];
            assert_eq!(indirect_descriptor_index, token);
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].len as usize,
                4 * size_of::<Descriptor>()
            );
            assert_eq!(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].flags,
                DescFlags::INDIRECT
            );

            let indirect_descriptors = slice_from_raw_parts(
                (*queue.desc.as_ptr())[indirect_descriptor_index as usize].addr
                    as *const Descriptor,
                4,
            );
            assert_eq!((*indirect_descriptors)[0].len, 2);
            assert_eq!((*indirect_descriptors)[0].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[0].next, 1);
            assert_eq!((*indirect_descriptors)[1].len, 1);
            assert_eq!((*indirect_descriptors)[1].flags, DescFlags::NEXT);
            assert_eq!((*indirect_descriptors)[1].next, 2);
            assert_eq!((*indirect_descriptors)[2].len, 2);
            assert_eq!(
                (*indirect_descriptors)[2].flags,
                DescFlags::NEXT | DescFlags::WRITE
            );
            assert_eq!((*indirect_descriptors)[2].next, 3);
            assert_eq!((*indirect_descriptors)[3].len, 1);
            assert_eq!((*indirect_descriptors)[3].flags, DescFlags::WRITE);
        }
    }

    /// Tests that the queue advises the device that notifications are needed.
    #[test]
    fn set_dev_notify() {
        let state = Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], ())));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            state: state.clone(),
        };
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();

        // Check that the avail ring's flag is zero by default.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );

        queue.set_dev_notify(false);

        // Check that the avail ring's flag is 1 after `disable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x1
        );

        queue.set_dev_notify(true);

        // Check that the avail ring's flag is 0 after `enable_dev_notify`.
        assert_eq!(
            unsafe { (*queue.avail.as_ptr()).flags.load(Ordering::Acquire) },
            0x0
        );
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications.
    #[test]
    fn add_notify() {
        let state = Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], ())));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: 0,
            state: state.clone(),
        };
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, false, false).unwrap();

        // Add a buffer chain with a single device-readable part.
        unsafe { queue.add(&[&[42]], &mut []) }.unwrap();

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr()).flags.store(0x01, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);
    }

    /// Tests that the queue notifies the device about added buffers, if it hasn't suppressed
    /// notifications with the `avail_event` index.
    #[test]
    fn add_notify_event_idx() {
        let state = Arc::new(Mutex::new(State::new(vec![QueueStatus::default()], ())));
        let mut transport = FakeTransport {
            device_type: DeviceType::Block,
            max_queue_size: 4,
            device_features: Feature::RING_EVENT_IDX.bits(),
            state: state.clone(),
        };
        let mut queue =
            VirtQueue::<FakeHal, 4>::new(&mut transport, 0, false, true, false).unwrap();

        // Add a buffer chain with a single device-readable part.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 0);

        // Check that the transport would be notified.
        assert_eq!(queue.should_notify(), true);

        // SAFETY: the various parts of the queue are properly aligned, dereferenceable and
        // initialised, and nothing else is accessing them at the same time.
        unsafe {
            // Suppress notifications.
            (*queue.used.as_ptr())
                .avail_event
                .store(1, Ordering::Release);
        }

        // Check that the transport would not be notified.
        assert_eq!(queue.should_notify(), false);

        // Add another buffer chain.
        assert_eq!(unsafe { queue.add(&[&[42]], &mut []) }.unwrap(), 1);

        // Check that the transport should be notified again now.
        assert_eq!(queue.should_notify(), true);
    }
}
