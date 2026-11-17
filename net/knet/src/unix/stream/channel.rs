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

use crate::{
    options::PeerCredentials,
    socket::{AncillaryData, KernelAncillaryData, SenderCredentials},
};

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
    /// The `SO_PASSCRED` setting of the socket that owns this endpoint.
    ///
    /// Both channel directions share the endpoint, so a sender can observe
    /// whether its peer enabled automatic credential capture. A queued server
    /// endpoint copies this from the listener when `connect()` builds it.
    pub(super) pass_credentials: AtomicBool,
    /// Whether this endpoint belongs to a server-side connection that has not
    /// been returned by `accept()` yet.
    ///
    /// Linux `unix_passcred_enabled` also requests credentials while the
    /// receiving peer has no attached socket (`!other->sk_socket`), so data
    /// sent before `accept()` must carry a snapshot even when the listener did
    /// not enable `SO_PASSCRED`.
    pub(super) awaiting_accept: AtomicBool,
}

impl StreamEndpoint {
    /// Whether this endpoint requests automatic sender credentials.
    ///
    /// This combines the endpoint's own `SO_PASSCRED` selection with the
    /// pre-accept capture rule for a queued server connection.
    pub(super) fn pass_credentials_enabled(&self) -> bool {
        self.pass_credentials.load(Ordering::Acquire)
            || self.awaiting_accept.load(Ordering::Acquire)
    }
}

pub(super) fn new_duplex_channel(
    client_endpoint: Arc<StreamEndpoint>,
    server_endpoint: Arc<StreamEndpoint>,
    client_peer: PeerCredentials,
    server_peer: PeerCredentials,
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
            peer_credentials: client_peer,
        },
        Channel {
            tx: server_tx,
            rx: server_rx,
            tx_anc: server_to_client_anc,
            rx_anc: client_to_server_anc,
            endpoint: server_endpoint,
            peer_endpoint: client_endpoint,
            peer_credentials: server_peer,
        },
    )
}

/// Ancillary records (e.g. SCM_RIGHTS file descriptions) in flight on one
/// stream direction. Shared between the sending and receiving endpoints so
/// that a record can be attached when the sender publishes its bytes and
/// released when the receiver consumes the first byte of that range.
///
/// Retention is bounded: at most [`MAX_PENDING_RECORDS`] records carrying at
/// most [`MAX_PENDING_ANCILLARY`] entries per direction. Every entry is one
/// `SCM_RIGHTS` control message whose descriptor count `posix-net` caps at
/// `SCM_MAX_FD` (253), so one record never pins more descriptors than a single
/// conforming `sendmsg` and the retained file references per direction are
/// bounded at `MAX_PENDING_RECORDS * SCM_MAX_FD`. Without the bound a local
/// peer could pin unbounded file references behind a slow consumer with
/// one-byte sends.
pub(super) const MAX_PENDING_RECORDS: usize = 256;
pub(super) const MAX_PENDING_ANCILLARY: usize = 1024;

/// Why the producer could not register a control record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EnqueueError {
    /// The consumer closed this direction; the record could never be
    /// delivered, so the send fails like a write to a closed peer.
    ConsumerClosed,
    /// The pending-record quota ([`MAX_PENDING_RECORDS`] records or
    /// [`MAX_PENDING_ANCILLARY`] retained entries) is exhausted. Only the
    /// consumer retiring a record frees capacity and that transition has no
    /// wakeup a sender could wait on, so a send beyond the quota fails with
    /// `ENOBUFS` immediately, leaving the round's bytes unpublished.
    QuotaExceeded,
}

/// Whether `payload` carries sender credentials, including a mixed record that
/// also carries file descriptors.
fn payload_contains_credentials(payload: &[AncillaryData]) -> bool {
    payload.iter().any(|record| {
        record
            .downcast_ref::<KernelAncillaryData>()
            .is_some_and(|data| matches!(data, KernelAncillaryData::Credentials(_)))
    })
}

/// The single credential record carried by `payload`, if it is exactly that.
///
/// A control record that also carries file descriptors is an `SCM_RIGHTS`
/// barrier and must not be merged or skipped as a credential-only record.
fn sender_credentials(payload: &[AncillaryData]) -> Option<&SenderCredentials> {
    let [record] = payload else {
        return None;
    };
    match record.downcast_ref::<KernelAncillaryData>()? {
        KernelAncillaryData::Credentials(credentials) => Some(credentials),
        _ => None,
    }
}

/// Linux compares the stored `struct pid` pointer, real UID and real GID when
/// deciding whether two adjacent stream skbs can be read together. The same
/// comparison lets us coalesce the corresponding credential records.
fn same_sender_credentials(left: &SenderCredentials, right: &SenderCredentials) -> bool {
    left.uid == right.uid
        && left.gid == right.gid
        && match (&left.pid, &right.pid) {
            (Some(left), Some(right)) => Arc::ptr_eq(left, right),
            (None, None) => true,
            _ => false,
        }
}

pub(super) struct DirectionAncillary {
    /// Total bytes ever produced on this direction (monotonic).
    pub(super) sent_total: u64,
    /// Total bytes ever consumed on this direction (monotonic).
    pub(super) recv_total: u64,
    /// Undelivered records ordered by the stream position of their first
    /// byte. Positions are in `sent_total`/`recv_total` units. A record that
    /// contains an `SCM_RIGHTS` payload is a read barrier: `end` is the
    /// stream position just past the sendmsg that carried the fds, and reads
    /// stop there even when later plain-data sends leave no record boundary
    /// to infer it from. Adjacent credential-only sends with equal identities
    /// share one record and are read together, like Linux coalesces skbs
    /// whose `unix_skb_scm_eq` holds.
    pub(super) records: VecDeque<(u64, u64, Arc<Vec<AncillaryData>>)>,
    /// The entries live behind one `Arc` so a `MSG_PEEK` duplicates the
    /// record by cloning a single handle: no entry buffer is copied or
    /// re-allocated while the control guard is held, and every delivered
    /// reference shares the peer's open file description.
    /// Sum of `record.2.len()` across pending records. Counts retained
    /// control messages and, together with the per-message `SCM_MAX_FD` cap,
    /// bounds the file references the queue holds for a consumer that has
    /// not caught up.
    retained_ancillary: usize,
    /// Set when the consumer closes its endpoint: the queue is drained then
    /// and later registrations are rejected, so a producer cannot pin file
    /// references behind a consumer that is gone.
    consumer_closed: bool,
}

impl DirectionAncillary {
    fn new() -> Self {
        Self {
            sent_total: 0,
            recv_total: 0,
            // Reserve the whole quota so registering a record never grows the
            // deque under the control guard. The cost is bounded at
            // `MAX_PENDING_RECORDS` entries per direction (a
            // `(u64, u64, Arc<Vec<_>>)` element is 24 bytes, ~6 KiB per
            // direction and ~12 KiB per duplex channel), small next to the
            // 64 KiB ring buffer of the same direction.
            records: VecDeque::with_capacity(MAX_PENDING_RECORDS),
            retained_ancillary: 0,
            consumer_closed: false,
        }
    }

    /// Admits one control record for the byte range `[start, end)`.
    ///
    /// `payload` is consumed; a rejected admission returns it to the caller
    /// together with the reason, so the payload can be dropped after the
    /// caller's spin guards are released (dropping the last file reference
    /// closes the file, which may take a sleepable mutex).
    ///
    /// A credential-only payload directly following another credential-only
    /// record for the same sender is merged into that record. Dropping the
    /// new payload here is safe because it cannot contain a file reference.
    pub(super) fn enqueue_record(
        &mut self,
        start: u64,
        end: u64,
        payload: Arc<Vec<AncillaryData>>,
    ) -> Result<(), (EnqueueError, Arc<Vec<AncillaryData>>)> {
        if self.consumer_closed {
            return Err((EnqueueError::ConsumerClosed, payload));
        }
        if let Some((_, last_end, last_payload)) = self.records.back_mut()
            && *last_end == start
            && let Some(last_credentials) = sender_credentials(last_payload)
            && let Some(credentials) = sender_credentials(&payload)
            && same_sender_credentials(last_credentials, credentials)
        {
            *last_end = end;
            return Ok(());
        }
        if self.records.len() >= MAX_PENDING_RECORDS
            || self.retained_ancillary + payload.len() > MAX_PENDING_ANCILLARY
        {
            return Err((EnqueueError::QuotaExceeded, payload));
        }
        debug_assert!(
            self.records.back().is_none_or(|last| last.1 <= start),
            "records must stay in ascending position order"
        );
        self.retained_ancillary += payload.len();
        self.records.push_back((start, end, payload));
        Ok(())
    }

    /// Returns the read boundary for a receive that may consume up to
    /// `occupied` bytes.
    ///
    /// Without `SO_PASSCRED` every credential-only record is filtered from the
    /// result, so it cannot shorten a read; the next `SCM_RIGHTS` record still
    /// does. With `SO_PASSCRED`, a record carrying credentials is a boundary at
    /// its merged end once its bytes are being consumed, matching Linux
    /// coalescing of equal adjacent skbs. Before that point the boundary is the
    /// record's start: earlier plain bytes have no matching credential record
    /// and must not be merged with this skb (`unix_stream_read_generic` stops
    /// before a skb whose credentials differ).
    pub(super) fn read_cap(&self, occupied: u64, pass_credentials: bool) -> u64 {
        for (start, end, payload) in &self.records {
            if !pass_credentials && sender_credentials(payload).is_some() {
                continue;
            }
            let boundary = if pass_credentials
                && payload_contains_credentials(payload)
                && self.recv_total < *start
            {
                *start
            } else {
                *end
            };
            return boundary.saturating_sub(self.recv_total).min(occupied);
        }
        occupied
    }

    /// Pops the records reached by the receive cursor and returns at most one
    /// payload to deliver.
    ///
    /// A credential-only record stays queued until all bytes it covers have
    /// been consumed, so a short read does not lose credentials for the bytes
    /// that remain. While the credential record is still partially unconsumed
    /// its payload is cloned for delivery (no allocation) and the shared
    /// record remains available for the next read or `MSG_PEEK`. Without
    /// `SO_PASSCRED`, fully consumed credential records are discarded without
    /// being returned; an `SCM_RIGHTS` record is returned even when it is the
    /// only record the read reached.
    pub(super) fn take_reached(
        &mut self,
        pass_credentials: bool,
    ) -> Option<(u64, u64, Arc<Vec<AncillaryData>>)> {
        loop {
            let (start, end, payload) = self.records.front()?;
            if *start >= self.recv_total {
                return None;
            }
            if sender_credentials(payload).is_some() {
                if self.recv_total < *end {
                    return pass_credentials.then(|| {
                        let start = *start;
                        let end = *end;
                        (start, end, Arc::clone(payload))
                    });
                }
                let (start, end, payload) = self.records.pop_front()?;
                self.retained_ancillary -= payload.len();
                if pass_credentials {
                    return Some((start, end, payload));
                }
                continue;
            }
            let (start, end, payload) = self.records.pop_front()?;
            self.retained_ancillary -= payload.len();
            return Some((start, end, payload));
        }
    }

    /// Duplicates the first record before `cursor_end` that this receiver
    /// would deliver, without touching the queue or the cursor: a `MSG_PEEK`
    /// shares the sender's entry handles (`scm_fp_dup`) and keeps the record
    /// for the later consuming receive. Credential-only records are skipped
    /// when the receiver did not enable `SO_PASSCRED`. Cloning the shared
    /// handle bumps reference counts and never allocates, so it stays inside
    /// the guard.
    pub(super) fn peek_reached(
        &self,
        cursor_end: u64,
        pass_credentials: bool,
    ) -> Option<Arc<Vec<AncillaryData>>> {
        for (start, _, payload) in &self.records {
            if *start >= cursor_end {
                return None;
            }
            if pass_credentials || sender_credentials(payload).is_none() {
                return Some(Arc::clone(payload));
            }
        }
        None
    }

    /// Closes this direction's consumer side: pending records can no longer
    /// be delivered, so they are drained and returned to the caller, and
    /// later registrations are rejected with
    /// [`EnqueueError::ConsumerClosed`].
    ///
    /// Draining is O(1) and performs no allocation, so it is safe under both
    /// the control guard and the caller's `tx_order` guard. The caller must
    /// drop the returned records after releasing its guards, because
    /// dropping the last reference of a passed file (for example a pipe write
    /// end) takes a sleepable mutex.
    pub(super) fn close_consumer(&mut self) -> VecDeque<(u64, u64, Arc<Vec<AncillaryData>>)> {
        self.consumer_closed = true;
        self.retained_ancillary = 0;
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
    pub(super) peer_credentials: PeerCredentials,
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
        // and release the fds without any spin guard held. Closing also
        // rejects later registrations: the peer's send observes rx_closed in
        // the same critical section and reports `EPIPE`.
        let (is_rx_changed, has_unread_input, dead) = {
            let _tx_order = self.peer_endpoint.tx_order.lock();
            let is_rx_changed = !self.endpoint.rx_closed.swap(true, Ordering::AcqRel);
            let has_unread_input = self.rx.occupied_len() > 0;
            let dead = self.rx_anc.lock().close_consumer();
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

unittest::path_mod_test!(tests, "../../../ut-tests/unix/stream/channel/tests.rs");
