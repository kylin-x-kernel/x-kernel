// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Connected Unix stream channel state.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    mem,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};

use kerrno::LinuxError;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet};
use kspin::SpinNoPreempt;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Observer, Split},
};

use crate::socket::AncillaryData;

pub(super) const STREAM_BUF_BYTES: usize = 64 * 1024;
pub(super) const STREAM_WRITABLE_MAX_OCCUPIED_BYTES: usize = STREAM_BUF_BYTES / 4;

pub(super) fn is_stream_writable(occupied_bytes: usize) -> bool {
    occupied_bytes <= STREAM_WRITABLE_MAX_OCCUPIED_BYTES
}

fn new_ring_pair() -> (HeapProd<u8>, HeapCons<u8>) {
    let rb = HeapRb::new(STREAM_BUF_BYTES);
    rb.split()
}

#[derive(Default)]
pub(super) struct StreamPollSets {
    pub(super) readable: PollSet,
    pub(super) writable: PollSet,
    state: PollSet,
}

impl StreamPollSets {
    pub(super) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let has_read_events =
            events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND | IoEvents::RDHUP);
        let has_write_events =
            events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND);
        if has_read_events {
            context.register(&self.readable)?;
        }
        if has_write_events {
            context.register(&self.writable)?;
        }
        if !has_read_events && !has_write_events && events.intersects(IoEvents::ERR | IoEvents::HUP)
        {
            context.register(&self.state)?;
        }
        Ok(())
    }

    pub(super) fn wake_state_change(&self) {
        self.readable.wake();
        self.writable.wake();
        self.state.wake();
    }
}

#[derive(Default)]
pub(super) struct StreamEndpoint {
    pub(super) polls: StreamPollSets,
    /// Orders data publication, shutdown, and EOF observation for this
    /// endpoint's transmit direction.
    pub(super) tx_order: SpinNoPreempt<()>,
    pub(super) rx_closed: AtomicBool,
    pub(super) tx_closed: AtomicBool,
    pub(super) socket_error: AtomicI32,
}

pub(super) fn new_duplex_channel(
    client_endpoint: Arc<StreamEndpoint>,
    server_endpoint: Arc<StreamEndpoint>,
    pid: u32,
) -> (Channel, Channel) {
    let (client_tx, server_rx) = new_ring_pair();
    let (server_tx, client_rx) = new_ring_pair();
    let client_to_server_anc = SharedAncillary::new(SpinNoPreempt::new(DirectionAncillary::new()));
    let server_to_client_anc = SharedAncillary::new(SpinNoPreempt::new(DirectionAncillary::new()));
    (
        Channel {
            tx: client_tx,
            rx: client_rx,
            tx_anc: client_to_server_anc.clone(),
            rx_anc: server_to_client_anc.clone(),
            endpoint: client_endpoint.clone(),
            peer_endpoint: server_endpoint.clone(),
            peer_pid: pid,
        },
        Channel {
            tx: server_tx,
            rx: server_rx,
            tx_anc: server_to_client_anc,
            rx_anc: client_to_server_anc,
            endpoint: server_endpoint,
            peer_endpoint: client_endpoint,
            peer_pid: pid,
        },
    )
}

/// Ancillary records (e.g. SCM_RIGHTS file descriptions) in flight on one
/// stream direction. Shared between the sending and receiving endpoints so
/// that a record can be attached when the sender publishes its bytes and
/// released when the receiver consumes the first byte of that range.
pub(super) struct DirectionAncillary {
    /// Total bytes ever produced on this direction (monotonic).
    pub(super) sent_total: u64,
    /// Total bytes ever consumed on this direction (monotonic).
    pub(super) recv_total: u64,
    /// Undelivered records ordered by the stream position of their first
    /// byte. Positions are in `sent_total`/`recv_total` units. `end` is the
    /// stream position just past the sendmsg that carried the fds: reads
    /// stop there (the SCM_RIGHTS barrier), even when later plain-data
    /// sends leave no record boundary to infer it from.
    pub(super) records: VecDeque<(u64, u64, Vec<AncillaryData>)>,
}

impl DirectionAncillary {
    fn new() -> Self {
        Self {
            sent_total: 0,
            recv_total: 0,
            records: VecDeque::new(),
        }
    }

    /// Pops the oldest ancillary record whose first byte the receive cursor
    /// has reached, or `None` when the cursor has not reached any record
    /// yet. A record whose first byte sits exactly at the cursor is not
    /// reached until the cursor moves past it.
    pub(super) fn pop_reached(&mut self) -> Option<(u64, u64, Vec<AncillaryData>)> {
        if self.records.front()?.0 >= self.recv_total {
            return None;
        }
        self.records.pop_front()
    }

    /// Takes every queued record regardless of the receive cursor: used when
    /// the receiving end is going away, so nothing will ever consume them.
    /// The queue is moved out wholesale (O(1), no allocation) so the caller
    /// can release the records outside its spin-guard sections; dropping
    /// them runs `VfsFile::release` and may block.
    pub(super) fn take_all(&mut self) -> VecDeque<(u64, u64, Vec<AncillaryData>)> {
        mem::take(&mut self.records)
    }
}

// Spin-only lock: send/recv poll closures must never park (poll_io contract),
// so the ancillary queue follows the same locking discipline as the ring
// buffer's `tx_order`. A blocking mutex here deadlocks-asserts the scheduler
// when two peers contend (`blocked_resched` sees a foreign preempt count).
pub(super) type SharedAncillary = Arc<SpinNoPreempt<DirectionAncillary>>;

pub(super) struct Channel {
    pub(super) tx: HeapProd<u8>,
    pub(super) rx: HeapCons<u8>,
    /// Ancillary queue for this channel's transmit direction: records are
    /// attached here by [`StreamTransport::send`](super::StreamTransport) as
    /// bytes are published.
    pub(super) tx_anc: SharedAncillary,
    /// Ancillary queue for this channel's receive direction: records attached
    /// by the peer are delivered from here by
    /// [`StreamTransport::recv`](super::StreamTransport) as bytes are consumed.
    pub(super) rx_anc: SharedAncillary,
    pub(super) endpoint: Arc<StreamEndpoint>,
    pub(super) peer_endpoint: Arc<StreamEndpoint>,
    pub(super) peer_pid: u32,
}

impl Drop for Channel {
    fn drop(&mut self) {
        // The peer still holds this direction's undelivered records through
        // the shared queue on its own tx_anc; its Channel keeps that Arc
        // alive, so the queued fds would stay pinned until the peer is
        // dropped (compounding T-42). Drain them inside the same critical
        // section that flags the close — the sender enqueues under the
        // peer's tx_order lock, so nothing can be enqueued afterwards — and
        // take the queue out (O(1), no allocation in the critical section)
        // and release the fds without any spin guard held.
        let (is_rx_changed, has_unread_input, dead) = {
            let _tx_order = self.peer_endpoint.tx_order.lock();
            let is_rx_changed = !self.endpoint.rx_closed.swap(true, Ordering::AcqRel);
            let has_unread_input = self.rx.occupied_len() > 0;
            let dead = self.rx_anc.lock().take_all();
            (is_rx_changed, has_unread_input, dead)
        };
        drop(dead);
        let is_tx_changed = {
            let _tx_order = self.endpoint.tx_order.lock();
            !self.endpoint.tx_closed.swap(true, Ordering::AcqRel)
        };
        if has_unread_input {
            self.peer_endpoint
                .socket_error
                .store(LinuxError::ECONNRESET.into_raw(), Ordering::Release);
        }
        if is_rx_changed || is_tx_changed || has_unread_input {
            self.endpoint.polls.wake_state_change();
            self.peer_endpoint.polls.wake_state_change();
        }
    }
}

#[unittest::mod_test]
mod tests {
    use alloc::{boxed::Box, collections::VecDeque, vec};

    use unittest::{assert, assert_eq, def_test};

    use super::DirectionAncillary;
    use crate::socket::AncillaryData;

    fn ancillary_queue(records: &[(u64, u64, u32)]) -> DirectionAncillary {
        let mut anc = DirectionAncillary {
            sent_total: 100,
            recv_total: 0,
            records: VecDeque::new(),
        };
        for (start, end, tag) in records {
            anc.records
                .push_back((*start, *end, vec![Box::new(*tag) as AncillaryData]));
        }
        anc
    }

    fn tag(record: &[AncillaryData]) -> u32 {
        *record[0].downcast_ref::<u32>().expect("single tag payload")
    }

    #[def_test]
    fn pop_reached_reached_requires_cursor_past_record_start() {
        let mut anc = ancillary_queue(&[(2, 3, 1)]);
        // cursor before the record start: not reached
        assert!(anc.pop_reached().is_none());
        // exactly at the record start: still not reached
        anc.recv_total = 2;
        assert!(anc.pop_reached().is_none());
        // past the start: reached, FIFO
        anc.recv_total = 3;
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
        assert!(anc.pop_reached().is_none());
    }

    #[def_test]
    fn pop_reached_pops_all_reached_records_in_order() {
        let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2), (9, 10, 3)]);
        anc.recv_total = 10;
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 3);
        assert!(anc.pop_reached().is_none());
    }

    #[def_test]
    fn pop_reached_stops_at_unreached_records() {
        let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2), (90, 91, 3)]);
        anc.recv_total = 10;

        // both reached records come out FIFO...
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
        // ...and the unreached one stays queued
        assert_eq!(anc.records.len(), 1);
        assert_eq!(anc.records.front().unwrap().0, 90);
    }

    #[def_test]
    fn pop_reached_drains_everything_when_all_is_reached() {
        let mut anc = ancillary_queue(&[(1, 2, 1), (5, 6, 2)]);
        anc.recv_total = 10;

        assert_eq!(tag(&anc.pop_reached().unwrap().2), 1);
        assert_eq!(tag(&anc.pop_reached().unwrap().2), 2);
        assert!(anc.records.is_empty());
    }
}
