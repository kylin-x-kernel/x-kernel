// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-owned requests. Boxes retain protocol and data memory until DMA retirement.

use alloc::{boxed::Box, vec};
use core::ptr::NonNull;

use block::{BioOperation, completion::BlockCompletionOperations};
use device_res::{IrqEvent, IrqHandler};
use linked_list_r4l::{GetLinks, Links, List};
use virtio_drivers::{
    Error,
    device::blk::{BlkReq, BlkResp, MAX_DATA_SEGMENTS},
};

use super::*;

struct VirtioRequest {
    links: Links<Self>,
    header: BlkReq,
    response: BlkResp,
    request: block::Request,
    // A single sector is staged only if hardware cannot describe its fragments.
    bounce: Option<Box<[u8]>>,
    result: DriverResult,
}

impl GetLinks for VirtioRequest {
    type EntryType = Self;

    fn get_links(data: &Self) -> &Links<Self> {
        &data.links
    }
}

impl VirtioRequest {
    fn new(mut request: block::Request, max_segments: usize) -> Box<Self> {
        let count = request.vectors_mut().len();
        let needs_bounce = count > max_segments
            && request
                .vectors_mut()
                .any(|vector| !vector.bytes().len().is_multiple_of(SECTOR_SIZE));
        Box::new(Self {
            links: Links::new(),
            header: BlkReq::default(),
            response: BlkResp::default(),
            request,
            bounce: needs_bounce.then(|| vec![0; SECTOR_SIZE].into_boxed_slice()),
            result: Ok(()),
        })
    }

    fn finish(self) {
        self.request.complete(self.result);
    }

    // VirtIOBlk::new bounds max_segments by MAX_DATA_SEGMENTS; this adapter's
    // init_device ensures max_size can hold the single-sector fallback.
    // Select a sector-aligned prefix fitting the actual device segment limits.
    // This is deterministic until advance(), so completion rebuilds exactly the
    // submitted descriptor list, without retaining a second payload allocation.
    fn transfer_segments(
        &mut self,
        max_segments: usize,
        max_size: usize,
        output: &mut [Option<NonNull<[u8]>>],
    ) -> (usize, usize) {
        let operation = self.request.operation();
        let (mut count, mut len) = (0, 0);
        for mut vector in self.request.vectors_mut() {
            let segment = if operation == BioOperation::Read {
                NonNull::from(vector.bytes_mut().expect("validated read extent"))
            } else {
                NonNull::from(vector.bytes())
            };
            let mut offset = 0;
            while offset < segment.len() && count < max_segments {
                let size = (segment.len() - offset).min(max_size);
                let data = segment.as_ptr().cast::<u8>().wrapping_add(offset);
                output[count] = Some(NonNull::slice_from_raw_parts(
                    NonNull::new(data).unwrap(),
                    size,
                ));
                count += 1;
                len += size;
                offset += size;
            }
            if count == max_segments {
                break;
            }
        }
        let mut tail = len % SECTOR_SIZE;
        len -= tail;
        while tail != 0 {
            let segment = output[count - 1].take().unwrap();
            if segment.len() <= tail {
                tail -= segment.len();
                count -= 1;
            } else {
                output[count - 1] = Some(NonNull::slice_from_raw_parts(
                    segment.cast(),
                    segment.len() - tail,
                ));
                tail = 0;
            }
        }
        (count, len)
    }

    fn copy_sector(&mut self, to_bounce: bool) {
        let bytes = self.bounce.as_mut().expect("fragmented sector staging");
        let mut copied = 0;
        for mut vector in self.request.vectors_mut() {
            let len = vector.bytes().len().min(SECTOR_SIZE - copied);
            if to_bounce {
                bytes[copied..copied + len].copy_from_slice(&vector.bytes()[..len]);
            } else {
                vector.bytes_mut().expect("validated read extent")[..len]
                    .copy_from_slice(&bytes[copied..copied + len]);
            }
            copied += len;
            if copied == SECTOR_SIZE {
                break;
            }
        }
        assert_eq!(copied, SECTOR_SIZE);
    }

    // The node is raw-owned across calls; the device lock excludes all competing
    // submit/reclaim accesses. complete is called only after peek_used.
    fn transfer<H: Hal, T: Transport>(
        &mut self,
        device: &mut InnerDev<H, T>,
        token: Option<u16>,
    ) -> Result<(Option<u16>, usize), Error> {
        if self.request.operation() == BioOperation::Flush {
            return match token {
                // SAFETY: the raw-owned node retains these protocol fields
                // through the later completion under the same device lock.
                None => unsafe { device.flush_nb(&mut self.header, &mut self.response) }
                    .map(|token| (token, 0)),
                Some(token) => {
                    // SAFETY: this is token's original storage after peek_used,
                    // and the device lock excludes another reclaimer.
                    unsafe { device.complete_flush(token, &self.header, &mut self.response) }
                        .map(|()| (None, 0))
                }
            };
        }
        let sector = usize::try_from(self.request.sector()).map_err(|_| Error::InvalidParam)?;
        let mut segments = [None; MAX_DATA_SEGMENTS];
        let (mut count, mut len) = self.transfer_segments(
            device.max_segments(),
            device.max_segment_size(),
            &mut segments,
        );
        let is_bounced = len == 0;
        if is_bounced {
            if token.is_none() && self.request.operation() == BioOperation::Write {
                self.copy_sector(true);
            }
            segments[0] = Some(NonNull::from(
                &mut **self.bounce.as_mut().expect("fragmented sector staging"),
            ));
            count = 1;
            len = SECTOR_SIZE;
        }
        let result = match self.request.operation() {
            BioOperation::Read => {
                let mut buffers: [&mut [u8]; MAX_DATA_SEGMENTS] =
                    core::array::from_fn(|_| &mut [][..]);
                for (buffer, segment) in buffers.iter_mut().zip(&mut segments[..count]) {
                    // SAFETY: validated read bios own these disjoint destinations;
                    // no hardware accesses them before submission or after peek_used.
                    *buffer = unsafe { segment.as_mut().unwrap().as_mut() };
                }
                match token {
                    // SAFETY: the raw-owned node retains disjoint protocol and
                    // data buffers until the exact descriptor list is reclaimed.
                    None => unsafe {
                        device.read_blocks_sg_nb(
                            sector,
                            &mut self.header,
                            &mut buffers[..count],
                            &mut self.response,
                        )
                    }
                    .map(Some),
                    // SAFETY: peek_used reported this token and these are its
                    // original buffers; the device lock serializes reclamation.
                    Some(token) => unsafe {
                        device.complete_read_blocks_sg(
                            token,
                            &self.header,
                            &mut buffers[..count],
                            &mut self.response,
                        )
                    }
                    .map(|()| None),
                }
            }
            BioOperation::Write => {
                let mut buffers = [&[][..]; MAX_DATA_SEGMENTS];
                for (buffer, segment) in buffers.iter_mut().zip(&segments[..count]) {
                    // SAFETY: write sources remain initialized and shared-only;
                    // request ownership retains them through descriptor retirement.
                    *buffer = unsafe { segment.unwrap().as_ref() };
                }
                match token {
                    // SAFETY: the raw-owned node retains disjoint protocol and
                    // data buffers until the exact descriptor list is reclaimed.
                    None => unsafe {
                        device.write_blocks_sg_nb(
                            sector,
                            &mut self.header,
                            &buffers[..count],
                            &mut self.response,
                        )
                    }
                    .map(Some),
                    // SAFETY: peek_used reported this token and these are its
                    // original buffers; the device lock serializes reclamation.
                    Some(token) => unsafe {
                        device.complete_write_blocks_sg(
                            token,
                            &self.header,
                            &buffers[..count],
                            &mut self.response,
                        )
                    }
                    .map(|()| None),
                }
            }
            BioOperation::Flush => unreachable!(),
        };
        if token.is_some()
            && is_bounced
            && result.is_ok()
            && self.request.operation() == BioOperation::Read
        {
            self.copy_sector(false);
        }
        result.map(|token| (token, len))
    }
}

/// Owns queued and submitted requests, including their protocol allocations.
pub(super) struct BlockRequests {
    pending: List<Box<VirtioRequest>>,
    // Each non-null token owns one Box transferred before DMA publication.
    in_flight: Box<[Option<NonNull<VirtioRequest>>]>,
}

impl BlockRequests {
    pub(super) fn new(descriptors: u16) -> Self {
        Self {
            pending: List::new(),
            in_flight: (0..descriptors).map(|_| None).collect(),
        }
    }

    pub(super) fn is_drained(&self) -> bool {
        self.pending.is_empty() && self.in_flight.iter().all(Option::is_none)
    }

    fn dispatch<H: Hal, T: Transport>(
        &mut self,
        device: &mut InnerDev<H, T>,
        completed: &mut List<Box<VirtioRequest>>,
    ) {
        while let Some(node) = self.pending.pop_front() {
            // End Box's exclusive ownership before exposing protocol addresses;
            // no Box move/retag occurs while hardware may access these bytes.
            let pointer = NonNull::new(Box::into_raw(node)).expect("non-null request allocation");
            // SAFETY: this is the freshly transferred unique Box, not yet submitted.
            // The device lock protects its transition into the owning token table.
            let node = unsafe { &mut *pointer.as_ptr() };
            let submitted = node.transfer(device, None).map(|(token, _)| token);
            match submitted {
                Err(Error::QueueFull) => {
                    // SAFETY: QueueFull publishes no descriptors. This pointer
                    // still owns the Box transferred above, with no device access.
                    self.pending
                        .push_front(unsafe { Box::from_raw(pointer.as_ptr()) });
                    break;
                }
                Ok(Some(token)) => {
                    assert!(
                        self.in_flight[usize::from(token)]
                            .replace(pointer)
                            .is_none()
                    );
                }
                result => {
                    node.result = result.map(|_| ()).map_err(as_driver_error);
                    // SAFETY: an error or no-op flush has no live DMA references;
                    // ownership was never transferred into the token table.
                    completed.push_back(unsafe { Box::from_raw(pointer.as_ptr()) });
                }
            }
        }
    }

    fn reclaim<H: Hal, T: Transport>(
        &mut self,
        device: &mut InnerDev<H, T>,
        completed: &mut List<Box<VirtioRequest>>,
    ) {
        // No replenishment until this finite used-ring batch has been reclaimed.
        while let Some(token) = device.peek_used() {
            let pointer = self.in_flight[usize::from(token)]
                .take()
                .expect("owned submitted token");
            // SAFETY: the token owns this allocation. peek_used ended device
            // writes; the held lock excludes other reclaimers. The raw owner is
            // retained until complete_* proves descriptor/mapping retirement.
            let node = unsafe { &mut *pointer.as_ptr() };
            let result = node.transfer(device, Some(token));
            node.result = match result {
                Ok((_, len)) => {
                    node.request
                        .advance(len)
                        .expect("retired sector-aligned prefix");
                    Ok(())
                }
                Err(Error::IoError) => Err(DriverError::Io),
                Err(Error::Unsupported) => Err(DriverError::Unsupported),
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: any other completion error \
                              leaves DMA reclamation ambiguous; panic aborts before freeing live \
                              buffers"
                )]
                Err(error) => panic!("virtio block descriptor reclamation failed: {error:?}"),
            };
            // SAFETY: complete_* retired the original buffers; reconstruct the
            // sole Box owned by the token and transfer it to terminal callbacks.
            let node = unsafe { Box::from_raw(pointer.as_ptr()) };
            if node.result.is_ok() && !node.request.is_data_complete() {
                self.pending.push_back(node);
            } else {
                completed.push_back(node);
            }
        }
    }
}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    fn finish_requests(&self, mut completed: List<Box<VirtioRequest>>) {
        while let Some(node) = completed.pop_front() {
            node.finish();
            let notification = {
                let mut state = self.state.lock();
                state.active_requests -= 1;
                if state.active_requests == 0 && !state.is_accepting {
                    state.drain_notification.take()
                } else {
                    None
                }
            };
            if let Some(notification) = notification {
                notification.notify_completion();
            }
        }
    }
}

impl<H: Hal, T: Transport> block::BlkMqOperations for VirtIoBlkDev<H, T> {
    fn queue_rq(&self, request: block::Request) {
        let max_segments = {
            let state = self.state.lock();
            let Some(device) = state.device.as_ref().filter(|_| state.is_accepting) else {
                drop(state);
                request.complete(Err(DriverError::Io));
                return;
            };
            device.max_segments()
        };
        let node = VirtioRequest::new(request, max_segments);
        let mut completed = List::new();
        {
            let mut state = self.state.lock();
            if !state.is_accepting {
                drop(state);
                node.request.complete(Err(DriverError::Io));
                return;
            }
            state.active_requests += 1;
            let BlockState {
                device,
                requests,
                is_irq_driven,
                ..
            } = &mut *state;
            let device = device.as_mut().expect("accepting transport");
            requests.pending.push_back(node);
            requests.dispatch(device, &mut completed);
            if !*is_irq_driven {
                // Early boot has no completion executor. Poll the same owned
                // queue to retirement; no second request or waiting object.
                while !requests.is_drained() {
                    requests.reclaim(device, &mut completed);
                    requests.dispatch(device, &mut completed);
                    core::hint::spin_loop();
                }
            }
        }
        self.finish_requests(completed);
    }
}

impl<H: Hal, T: Transport> IrqHandler for VirtIoBlkDev<H, T> {
    fn handle(&self, _irq: usize) -> IrqEvent {
        if self
            .state
            .lock()
            .device
            .as_mut()
            .is_none_or(|device| device.ack_interrupt().is_empty())
        {
            IrqEvent::NOT_HANDLED
        } else {
            IrqEvent::HANDLED
        }
    }
}

impl<H: Hal, T: Transport> BlockCompletionOperations for VirtIoBlkDev<H, T> {
    fn process_completed_requests(&self) {
        let mut completed = List::new();
        {
            let mut state = self.state.lock();
            let BlockState {
                device, requests, ..
            } = &mut *state;
            let Some(device) = device else {
                return;
            };
            requests.reclaim(device, &mut completed);
            // Replenishment uses existing request nodes and nonblocking transport
            // operations. No submitting task has to wake just to advance the FIFO.
            requests.dispatch(device, &mut completed);
        }
        self.finish_requests(completed);
    }
}

unittest::path_mod_test!(
    pub(super) mod requests_unittest,
    "../../ut-tests/blk_requests_unittest.rs",
);
