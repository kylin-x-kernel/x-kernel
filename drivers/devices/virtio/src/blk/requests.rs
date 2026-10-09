// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Call-owned requests; all list, token and protocol access uses the device lock.
//! The only lifetime-erased pointers stay here. Synchronous entry points retain
//! the borrowed buffers and pinned nodes until every queue reference is removed.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::{cell::UnsafeCell, marker::PhantomPinned, pin::pin, ptr::NonNull};

use block::completion::{BlockCompletionOperations, BlockSignals, BlockWaiter, PrepareBlockWait};
use device_res::{IrqEvent, IrqHandler};
use linked_list_r4l::{GetLinks, Links, RawList};
use virtio_drivers::{
    Error,
    device::blk::{BlkReq, BlkResp},
};

use super::*;

#[derive(Clone, Copy)]
enum Operation {
    Read(usize, NonNull<[u8]>),
    Write(usize, NonNull<[u8]>),
    Flush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequestState {
    Prepared,
    Pending,
    Submitted(u16),
    Done(DriverResult),
}

struct Request {
    links: Links<Self>,
    header: UnsafeCell<BlkReq>,
    response: UnsafeCell<BlkResp>,
    operation: UnsafeCell<Option<Operation>>,
    state: UnsafeCell<RequestState>,
    signals: Arc<dyn BlockSignals>,
    _pin: PhantomPinned,
}

impl Request {
    fn new(operation: Operation, signals: Arc<dyn BlockSignals>) -> Self {
        Self {
            links: Links::new(),
            header: UnsafeCell::new(BlkReq::default()),
            response: UnsafeCell::new(BlkResp::default()),
            operation: UnsafeCell::new(Some(operation)),
            state: UnsafeCell::new(RequestState::Prepared),
            signals,
            _pin: PhantomPinned,
        }
    }
}

impl GetLinks for Request {
    type EntryType = Self;

    fn get_links(data: &Self) -> &Links<Self> {
        &data.links
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        // Normal exits have no surviving list/token/callback references. Kernel
        // panic aborts: unwinding a node through live DMA is not supported.
        assert!(matches!(
            self.state.get_mut(),
            RequestState::Prepared | RequestState::Done(_)
        ));
    }
}

/// Device-owned admission and token index; it never owns a caller's request.
pub(super) struct BlockRequests {
    pending: RawList<Request>,
    in_flight: Box<[Option<NonNull<Request>>]>,
    // Taken only by the host's serialized per-device completion callback.
    notifications: Option<Vec<Arc<dyn BlockSignals>>>,
}

impl BlockRequests {
    pub(super) fn is_drained(&self) -> bool {
        self.pending.is_empty()
            && self.in_flight.iter().all(Option::is_none)
            && self.notifications.as_ref().is_some_and(Vec::is_empty)
    }

    pub(super) fn new(descriptors: u16) -> Self {
        Self {
            pending: RawList::new(),
            in_flight: vec![None; usize::from(descriptors)].into_boxed_slice(),
            notifications: Some(Vec::with_capacity(usize::from(descriptors))),
        }
    }

    fn head_signal(&self) -> Option<Arc<dyn BlockSignals>> {
        self.pending
            .iter()
            .next()
            .map(|request| request.signals.clone())
    }

    /// # Safety
    /// The caller pins request until it is unlinked and its token is reclaimed.
    /// All request/list accesses, including terminal reads, hold the device lock.
    unsafe fn enqueue(&mut self, request: &Request) {
        // SAFETY: the caller keeps the node pinned, live and exclusively on this
        // FIFO; the device lock excludes other list/state access.
        assert_eq!(unsafe { *request.state.get() }, RequestState::Prepared);
        // SAFETY: The caller pins and retains this node through removal;
        // the held device lock serializes its link and pending-list accesses.
        assert!(unsafe { self.pending.push_back(NonNull::from(request)) });
        // SAFETY: The device lock still excludes other state readers/writers.
        unsafe { *request.state.get() = RequestState::Pending };
    }

    fn finish(request: &Request, result: DriverResult) {
        // SAFETY: callers hold the device lock and have removed all list/token
        // references after successful reclamation, or before any submission.
        unsafe { *request.operation.get() = None };
        // SAFETY: The same device lock protects state publication after all
        // list/token references have been reclaimed or before submission.
        unsafe { *request.state.get() = RequestState::Done(result) };
    }

    fn cancel_pending(&mut self, request: &Request, error: DriverError) {
        // SAFETY: only the submitting caller can cancel; it has not submitted
        // this Pending node, and holds its device lock and pinned lifetime.
        assert_eq!(unsafe { *request.state.get() }, RequestState::Pending);
        // SAFETY: This unsubmitted node belongs to pending and remains pinned;
        // the submitting caller holds the device lock throughout removal.
        assert!(unsafe { self.pending.remove(request) });
        Self::finish(request, Err(error));
    }

    fn try_submit<H: Hal, T: Transport>(
        &mut self,
        device: &mut InnerDev<H, T>,
        request: &Request,
    ) -> bool {
        if !self
            .pending
            .iter()
            .next()
            .is_some_and(|head| core::ptr::eq(head, request))
        {
            return false;
        }
        // SAFETY: run_request pins and retains request, and the device lock
        // serializes all state reads and transitions.
        assert_eq!(unsafe { *request.state.get() }, RequestState::Pending);
        // SAFETY: The same lock protects the initialized operation metadata.
        let operation = unsafe { *request.operation.get() }.expect("pending operation");
        // SAFETY: No device owns this pending request's header yet. Only this
        // field is borrowed, under the device lock, from the pinned request.
        let header = unsafe { &mut *request.header.get() };
        // SAFETY: response is a distinct initialized field not yet owned by
        // DMA; the device lock excludes any other CPU access.
        let response = unsafe { &mut *request.response.get() };
        let submitted = match operation {
            Operation::Read(sector, mut buffer) => {
                // SAFETY: run_request retains the exclusively borrowed read
                // buffer until terminal completion; no device owns it yet.
                let buffer = unsafe { buffer.as_mut() };
                // SAFETY: All three disjoint buffers stay pinned/live through
                // reclamation; the device lock prevents completion before the
                // returned token is installed below.
                unsafe { device.read_blocks_nb(sector, header, buffer, response) }.map(Some)
            }
            Operation::Write(sector, buffer) => {
                // SAFETY: run_request retains the immutable initialized write
                // buffer through terminal completion.
                let buffer = unsafe { buffer.as_ref() };
                // SAFETY: The header, data and response stay live and disjoint
                // until token reclamation, serialized by the held device lock.
                unsafe { device.write_blocks_nb(sector, header, buffer, response) }.map(Some)
            }
            Operation::Flush => {
                // SAFETY: Both disjoint protocol fields stay pinned/live through
                // completion, which cannot acquire the device lock until submit ends.
                unsafe { device.flush_nb(header, response) }
            }
        };
        if submitted == Err(Error::QueueFull) {
            return false;
        }
        assert_eq!(self.pending.pop_front(), Some(NonNull::from(request)));
        match submitted {
            Ok(Some(token)) => {
                let entry = self
                    .in_flight
                    .get_mut(usize::from(token))
                    .expect("valid submitted token");
                assert!(entry.replace(NonNull::from(request)).is_none());
                // SAFETY: the device lock protects this transition; token and
                // state are installed before completion can acquire the lock.
                unsafe {
                    *request.state.get() = RequestState::Submitted(token);
                }
            }
            Ok(None) => Self::finish(request, Ok(())),
            Err(error) => Self::finish(request, Err(as_driver_error(error))),
        }
        true
    }

    fn reclaim<H: Hal, T: Transport>(
        &mut self,
        device: &mut InnerDev<H, T>,
        signals: &mut Vec<Arc<dyn BlockSignals>>,
    ) {
        // Holding the device lock prevents replenishment: at most one pass per
        // currently submitted request, regardless of the software FIFO length.
        while let Some(token) = device.peek_used() {
            let entry = self
                .in_flight
                .get_mut(usize::from(token))
                .expect("used token in range");
            let pointer = entry.expect("used token belongs to a submitted request");
            // SAFETY: the token references a pinned caller whose terminal wait
            // cannot return yet. Device lock protects the node and its metadata.
            let request = unsafe { pointer.as_ref() };
            // SAFETY: The device lock serializes reads of this retained request.
            let state = unsafe { *request.state.get() };
            assert_eq!(state, RequestState::Submitted(token));
            // SAFETY: The submitted operation is initialized and immutable until
            // reclamation under the same device lock completes.
            let operation = unsafe { *request.operation.get() }.expect("submitted operation");
            // SAFETY: peek_used observed completion; the pinned header is the
            // one supplied for token and no writer can access it under this lock.
            let header = unsafe { &*request.header.get() };
            // SAFETY: The device finished writing this distinct response field,
            // and the device lock excludes concurrent CPU access.
            let response = unsafe { &mut *request.response.get() };
            let completed = match operation {
                Operation::Read(_, mut buffer) => {
                    // SAFETY: run_request retains this exclusive read buffer;
                    // peek_used observed that device writes have completed.
                    let buffer = unsafe { buffer.as_mut() };
                    // SAFETY: token and all buffers are exactly those submitted;
                    // peek_used reported completion and this lock serializes reclaim.
                    unsafe { device.complete_read_blocks(token, header, buffer, response) }
                }
                Operation::Write(_, buffer) => {
                    // SAFETY: run_request retains the initialized immutable
                    // write buffer until this terminal reclamation completes.
                    let buffer = unsafe { buffer.as_ref() };
                    // SAFETY: These are token's original submitted buffers;
                    // completion was observed and reclamation holds the device lock.
                    unsafe { device.complete_write_blocks(token, header, buffer, response) }
                }
                Operation::Flush => {
                    // SAFETY: These are the original protocol fields for the
                    // completed token, retained through this serialized reclamation.
                    unsafe { device.complete_flush(token, header, response) }
                }
            };
            // These are the only terminal statuses after successful pop_used in
            // the pinned dependency. NotReady is ambiguous and must fail closed.
            let result = match completed {
                Ok(()) => Ok(()),
                Err(Error::IoError) => Err(DriverError::Io),
                Err(Error::Unsupported) => Err(DriverError::Unsupported),
                #[expect(
                    clippy::panic,
                    reason = "CG-021-no-panic-on-reachable-failure: After a successful pop_used \
                              for this token, the pinned virtio-drivers completion returns only \
                              Ok, IoError or Unsupported; any other error means the descriptor \
                              chain cannot be reclaimed safely"
                )]
                Err(error) => panic!("virtio block descriptor reclamation failed: {error:?}"),
            };
            *entry = None;
            assert!(signals.len() < signals.capacity());
            signals.push(request.signals.clone());
            Self::finish(request, result);
        }
    }
}

impl<H: Hal, T: Transport> VirtIoBlkDev<H, T> {
    fn checked_sector(&self, sector: u64, len: usize) -> DriverResult<usize> {
        if len == 0
            || !len.is_multiple_of(SECTOR_SIZE)
            || sector
                .checked_add((len / SECTOR_SIZE) as u64)
                .is_none_or(|end| end > self.num_blocks)
        {
            return Err(DriverError::InvalidInput);
        }
        usize::try_from(sector).map_err(|_| DriverError::InvalidInput)
    }

    pub(super) fn read_request(
        &self,
        sector: u64,
        buffer: &mut [u8],
        prepare: PrepareBlockWait,
    ) -> DriverResult {
        let sector = self.checked_sector(sector, buffer.len())?;
        self.run_request(
            Operation::Read(sector, NonNull::from(buffer)),
            prepare()?.as_mut(),
        )
    }

    pub(super) fn write_request(
        &self,
        sector: u64,
        buffer: &[u8],
        prepare: PrepareBlockWait,
    ) -> DriverResult {
        if self.is_inherently_read_only() {
            return Err(DriverError::ReadOnly);
        }
        let sector = self.checked_sector(sector, buffer.len())?;
        self.run_request(
            Operation::Write(sector, NonNull::from(buffer)),
            prepare()?.as_mut(),
        )
    }

    pub(super) fn flush_request(&self, prepare: PrepareBlockWait) -> DriverResult {
        self.run_request(Operation::Flush, prepare()?.as_mut())
    }

    // Only the synchronous borrowed-buffer entry points above construct an
    // Operation. The pointer cannot escape this call's pinned-node lifetime.
    fn run_request(&self, operation: Operation, waiter: &mut dyn BlockWaiter) -> DriverResult {
        let request = pin!(Request::new(operation, waiter.signals()));
        let request = request.as_ref().get_ref();
        {
            let mut state = self.state.lock();
            // SAFETY: request is pinned in this frame and every normal exit
            // below first removes it from the FIFO/token table. Panics abort.
            unsafe {
                state
                    .requests
                    .as_mut()
                    .expect("IRQ request queue")
                    .enqueue(request);
            }
        }
        loop {
            let (submitted, next) = {
                let mut state = self.state.lock();
                let BlockState {
                    device, requests, ..
                } = &mut *state;
                let requests = requests.as_mut().unwrap();
                let submitted = requests
                    .try_submit(device.as_mut().expect("admitted block transport"), request);
                (
                    submitted,
                    if submitted {
                        requests.head_signal()
                    } else {
                        None
                    },
                )
            };
            if let Some(next) = next {
                next.notify_admission();
            }
            if submitted {
                break;
            }
            if let Err(error) = waiter.wait_admission() {
                let next = {
                    let mut state = self.state.lock();
                    let requests = state.requests.as_mut().unwrap();
                    requests.cancel_pending(request, error);
                    requests.head_signal()
                };
                if let Some(next) = next {
                    next.notify_admission();
                }
                return Err(error);
            }
        }
        loop {
            let result = {
                let _state = self.state.lock();
                // SAFETY: the device lock protects terminal publication and
                // includes retirement of all list/token/buffer references.
                match unsafe { *request.state.get() } {
                    RequestState::Done(result) => Some(result),
                    RequestState::Submitted(_) => None,
                    _ => unreachable!("request left admission without a result or token"),
                }
            };
            if let Some(result) = result {
                return result;
            }
            waiter.wait_completion();
        }
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
        let (mut signals, head) = {
            let mut state = self.state.lock();
            let BlockState {
                device, requests, ..
            } = &mut *state;
            let (Some(device), Some(requests)) = (device, requests) else {
                return;
            };
            let mut signals = requests
                .notifications
                .take()
                .expect("serialized device completion callback");
            requests.reclaim(device, &mut signals);
            (signals, requests.head_signal())
        };
        for signal in signals.drain(..) {
            signal.notify_completion();
        }
        if let Some(head) = head {
            head.notify_admission();
        }
        self.state.lock().requests.as_mut().unwrap().notifications = Some(signals);
    }
}

#[cfg(unittest)]
#[path = "tests/requests.rs"]
pub(super) mod tests;
