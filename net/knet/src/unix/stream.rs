// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix stream socket transport.

mod channel;
mod listener;

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use async_trait::async_trait;
use kerrno::{KError, KResult, LinuxError};
use kio::{IoBuf, IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use ksync::Mutex;
use ringbuf::traits::{Consumer, Observer, Producer};

pub(crate) use self::listener::Bind;
use self::{
    channel::{Channel, StreamEndpoint},
    listener::{ConnRequest, ListenerQueue},
};
use crate::{
    AncillaryData, ConnectOptions, RecvFlags, RecvOptions, SendOptions, Shutdown,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

pub struct StreamTransport {
    channel: Mutex<Option<Channel>>,
    listener: Mutex<Option<Arc<ListenerQueue>>>,
    /// Handle to the `BindEntry` stream slot. The slot is cleared on drop so
    /// stale addresses refuse new connections and pending requests are freed.
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
    endpoint: Arc<StreamEndpoint>,
    options: GeneralOptions,
    pid: u32,
}
impl StreamTransport {
    pub fn new(pid: u32) -> Self {
        StreamTransport::new_channel(None, pid)
    }

    fn new_channel(channel: Option<Channel>, pid: u32) -> Self {
        let endpoint = channel
            .as_ref()
            .map(|channel| channel.endpoint.clone())
            .unwrap_or_default();
        StreamTransport {
            channel: Mutex::new(channel),
            listener: Mutex::new(None),
            bind_slot: Mutex::new(None),
            endpoint,
            options: GeneralOptions::default(),
            pid,
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let endpoint1 = Arc::new(StreamEndpoint::default());
        let endpoint2 = Arc::new(StreamEndpoint::default());
        let (chan1, chan2) = channel::new_duplex_channel(endpoint1, endpoint2, pid);
        let transport1 = StreamTransport::new_channel(Some(chan1), pid);
        let transport2 = StreamTransport::new_channel(Some(chan2), pid);
        (transport1, transport2)
    }
}

impl Configurable for StreamTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        if let O::Error(error) = opt {
            **error = self.endpoint.socket_error.swap(0, Ordering::AcqRel);
            return Ok(OptionHandled::Yes);
        }
        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::SendBuffer(size) => {
                **size = channel::STREAM_BUF_BYTES;
            }
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                let peer_pid = self
                    .channel
                    .lock()
                    .as_ref()
                    .map_or(self.pid, |chan| chan.peer_pid);
                **cred = UnixCredentials::new(peer_pid);
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        if self.options.set_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
#[async_trait]
impl UnixTransportOps for StreamTransport {
    fn bind(&self, entry: &super::BindEntry, _local_addr: &UnixAddr) -> KResult<()> {
        let channel = self.channel.lock();
        if channel.is_some() {
            return Err(KError::InvalidInput);
        }
        let mut listener_guard = self.listener.lock();
        if listener_guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let bind_slot_handle = entry.stream.clone();
        let mut slot = entry.stream.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let listener = Arc::new(ListenerQueue::new(self.endpoint.clone()));
        *slot = Some(Bind {
            listener: listener.clone(),
            pid: self.pid,
        });
        drop(slot);
        *listener_guard = Some(listener);
        drop(listener_guard);
        drop(channel);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.endpoint.polls.wake_state_change();
        Ok(())
    }

    fn listen(&self, backlog: usize) -> KResult<()> {
        let channel = self.channel.lock();
        if channel.is_some() {
            return Err(KError::InvalidInput);
        }
        let listener = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        let (became_listening, capacity_increased) = listener.configure_listen(backlog);
        drop(channel);
        listener.notify_listen_change(became_listening, capacity_increased);
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindEntry,
        local_addr: &UnixAddr,
        options: ConnectOptions,
    ) -> KResult<()> {
        let bind = slot
            .stream
            .lock()
            .as_ref()
            .cloned()
            .ok_or(KError::ConnectionRefused)?;
        let is_nonblocking = self.options.nonblocking() || options.nonblocking;
        let reservation = bind
            .listener
            .reserve(is_nonblocking, self.options.send_timeout())?;

        let mut channel = self.channel.lock();
        if channel.is_some() {
            return Err(KError::AlreadyConnected);
        }
        if self
            .listener
            .lock()
            .as_ref()
            .is_some_and(|listener| listener.is_listening())
        {
            return Err(KError::InvalidInput);
        }

        let server_endpoint = Arc::new(StreamEndpoint::default());
        let (mut client_channel, mut server_channel) =
            channel::new_duplex_channel(self.endpoint.clone(), server_endpoint, 0);
        client_channel.peer_pid = bind.pid;
        server_channel.peer_pid = self.pid;
        let commit_result = reservation.commit(ConnRequest {
            channel: server_channel,
            addr: local_addr.clone(),
            pid: self.pid,
        });
        if let Err(error) = commit_result {
            drop(channel);
            bind.listener.notify_capacity_available();
            return Err(error);
        }
        *channel = Some(client_channel);
        drop(channel);
        bind.listener.notify_request_available();
        self.endpoint.polls.wake_state_change();
        Ok(())
    }

    async fn accept(&self, nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        let listener = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        let request = listener.accept(nonblocking).await?;
        let ConnRequest {
            channel,
            addr: peer_addr,
            pid,
        } = request;
        Ok((
            UnixTransport::Stream(StreamTransport::new_channel(Some(channel), pid)),
            peer_addr,
        ))
    }

    fn send(&self, mut src: impl Read + IoBuf, mut options: SendOptions) -> KResult<usize> {
        if options.to.is_some() {
            return Err(KError::InvalidInput);
        }
        let size = src.remaining();
        let mut total = 0;
        let non_blocking = self.options.nonblocking() || options.flags.nonblocking();
        self.options
            .send_poller_with_nonblocking(self, non_blocking, || {
                if self.endpoint.tx_closed.load(Ordering::Acquire) {
                    return finish_send_on_error(total, KError::BrokenPipe);
                }
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };
                {
                    let _tx_order = self.endpoint.tx_order.lock();
                    if self.endpoint.tx_closed.load(Ordering::Acquire)
                        || chan.peer_endpoint.rx_closed.load(Ordering::Acquire)
                        || !chan.tx.read_is_held()
                    {
                        return finish_send_on_error(total, KError::BrokenPipe);
                    }
                }

                let count = {
                    let (left, right) = chan.tx.vacant_slices_mut();
                    // SAFETY: The slices returned by `vacant_slices_mut` describe
                    // writable ring-buffer capacity. `Read::read` initializes only
                    // bytes it reports as written, and those bytes are published by
                    // `advance_write_index` below while the channel lock is held.
                    let mut count = src.read(unsafe { left.assume_init_mut() })?;
                    if count >= left.len() {
                        // SAFETY: Same invariant as for `left`; this slice is the
                        // second contiguous writable region of the same producer.
                        count += src.read(unsafe { right.assume_init_mut() })?;
                    }
                    count
                };
                // Take the record payload before taking any lock: `mem::take`
                // is O(1) and the option doubles as the registration state, so
                // a send replayed over several poll rounds registers its
                // record exactly once. A zero-byte round keeps the payload for
                // the round that actually publishes bytes; a send that never
                // publishes drops it with `options`, as with Linux stream
                // sendmsg (no skb is queued and the cmsg fds are released).
                let mut pending = (count > 0 && !options.ancillary.is_empty())
                    .then(|| Arc::new(core::mem::take(&mut options.ancillary)));
                // Attach ancillary records BEFORE publishing the bytes: a
                // concurrent receiver cannot consume unpublished bytes, so
                // enqueue-then-publish makes a record visible no later than
                // its first byte. Publishing first would let an SMP receiver
                // consume the range before the record is enqueued, deferring
                // or losing the fd. A rejected admission leaves the round's
                // bytes unpublished: the send fails (or returns the partial
                // `total`) and the caller retries from there.
                let rejection = {
                    let _tx_order = self.endpoint.tx_order.lock();
                    if self.endpoint.tx_closed.load(Ordering::Acquire)
                        || chan.peer_endpoint.rx_closed.load(Ordering::Acquire)
                        || !chan.tx.read_is_held()
                    {
                        return finish_send_on_error(total, KError::BrokenPipe);
                    }
                    let rejection = if count > 0 {
                        register_pending(chan, &mut pending, count).err()
                    } else {
                        None
                    };
                    if rejection.is_none() {
                        // SAFETY: `count` is the sum of bytes written into the vacant
                        // slices above, so it never exceeds the producer capacity that
                        // was exposed while the channel lock excluded other producers.
                        unsafe { chan.tx.advance_write_index(count) };
                    }
                    rejection
                };
                // Both guards are gone: the rejected payload drops here,
                // where releasing the last file reference may close a file
                // that takes a sleepable mutex.
                if let Some((error, payload)) = rejection {
                    drop(payload);
                    return match error {
                        channel::EnqueueError::ConsumerClosed => {
                            finish_send_on_error(total, KError::BrokenPipe)
                        }
                        channel::EnqueueError::QuotaExceeded => {
                            finish_send_on_error(total, KError::from(LinuxError::ENOBUFS))
                        }
                    };
                }
                total += count;
                if count > 0 {
                    chan.peer_endpoint.polls.readable.wake();
                }

                if total == size || (non_blocking && total > 0) {
                    Ok(total)
                } else {
                    Err(KError::WouldBlock)
                }
            })
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        let is_zero_length = dst.remaining_mut() == 0;
        let is_peek = options.flags.contains(RecvFlags::PEEK);
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };

                let occupied_before = chan.rx.occupied_len();
                if is_zero_length && occupied_before > 0 {
                    return Ok(0);
                }
                // SCM_RIGHTS barrier (Linux): this read stops at the end
                // of the first unconsumed fd-bearing segment — the record's
                // own committed range [start, end) — leaving anything
                // beyond it (plain data and later records alike) for the
                // next recvmsg. Without queued records the read is
                // unbounded. Cap the read at that end: Linux ends the read
                // after the fd-carrying sendmsg, so plain data following it
                // must not be consumed here even when later sends leave no
                // record boundary.
                let read_cap = {
                    let anc = chan.rx_anc.lock();
                    match anc.records.front() {
                        Some((_, end, _)) => (*end)
                            .saturating_sub(anc.recv_total)
                            .min(occupied_before as u64),
                        _ => occupied_before as u64,
                    }
                };
                let count = {
                    let (left, right) = chan.rx.as_slices();
                    let cap = (read_cap as usize).min(occupied_before);
                    let (left, right) = if left.len() >= cap {
                        (&left[..cap], &left[..0])
                    } else {
                        (left, &right[..cap - left.len()])
                    };
                    let mut count = dst.write(left)?;
                    if count >= left.len() {
                        count += dst.write(right)?;
                    }
                    if !is_peek {
                        // SAFETY: `count` is the sum of bytes copied out of the
                        // occupied slices returned by `as_slices`, bounded by the
                        // SCM_RIGHTS barrier below the next record's segment end,
                        // so advancing by this amount stays within the consumer's
                        // readable region. A peek never reaches this line: it
                        // copies data and control references without moving the
                        // cursor.
                        unsafe { chan.rx.advance_read_index(count) };
                    }
                    count
                };
                if count > 0 {
                    // A peek preserves the byte stream: it does not advance
                    // the read index, update the receive cursor, or wake the
                    // peer's writable poll set.
                    if !is_peek {
                        let occupied_after = occupied_before - count;
                        if !channel::is_stream_writable(occupied_before)
                            && channel::is_stream_writable(occupied_after)
                        {
                            chan.peer_endpoint.polls.writable.wake();
                        }
                        // Pop the (at most one, barrier-capped) reached record
                        // under the spin guard — deque pops only. The barrier
                        // capped the read at the first fd-bearing segment, so
                        // at most one record is reached: its entries go to the
                        // sink (or the record is disposed) together with the
                        // segment this recvmsg consumed.
                        let popped = {
                            let mut anc = chan.rx_anc.lock();
                            anc.recv_total += count as u64;
                            anc.pop_reached()
                        };
                        deliver_reached(popped, &mut options);
                    } else {
                        // A peek duplicates the record it touched: the shared
                        // entry handles go to the sink (`scm_fp_dup`) and the
                        // record stays queued for the later consuming recv.
                        // Without a control buffer the duplication cannot be
                        // emitted, so the loss is reported even though the
                        // record is kept.
                        let touched = {
                            let anc = chan.rx_anc.lock();
                            let cursor_end = anc.recv_total + count as u64;
                            anc.peek_reached(cursor_end)
                        };
                        deliver_peeked(touched, &mut options);
                    }
                    return Ok(count);
                }
                {
                    let _tx_order = chan.peer_endpoint.tx_order.lock();
                    if chan.rx.occupied_len() == 0 {
                        let error = self.endpoint.socket_error.swap(0, Ordering::AcqRel);
                        if error != 0 {
                            return Err(KError::from(LinuxError::new(error)).canonicalize());
                        }
                        if self.endpoint.rx_closed.load(Ordering::Acquire)
                            || chan.peer_endpoint.tx_closed.load(Ordering::Acquire)
                            || !chan.rx.write_is_held()
                        {
                            return Ok(0);
                        }
                    }
                }
                Err(KError::WouldBlock)
            })
    }

    fn shutdown(&self, how: Shutdown) -> KResult<()> {
        let channel = self.channel.lock();
        let listener = how
            .has_read()
            .then(|| self.listener.lock().clone())
            .flatten();
        let is_listener_shutdown_changed = listener
            .as_ref()
            .is_some_and(|listener| listener.mark_receive_shutdown());
        let is_rx_changed = if how.has_read() {
            let _tx_order = channel
                .as_ref()
                .map(|channel| channel.peer_endpoint.tx_order.lock());
            !self.endpoint.rx_closed.swap(true, Ordering::AcqRel)
        } else {
            false
        };
        let is_tx_changed = if how.has_write() {
            let _tx_order = self.endpoint.tx_order.lock();
            !self.endpoint.tx_closed.swap(true, Ordering::AcqRel)
        } else {
            false
        };
        let peer_endpoint = channel
            .as_ref()
            .map(|channel| channel.peer_endpoint.clone());
        let is_hung_up = peer_endpoint.as_ref().map_or_else(
            || {
                self.endpoint.rx_closed.load(Ordering::Acquire)
                    && self.endpoint.tx_closed.load(Ordering::Acquire)
            },
            |peer_endpoint| {
                (self.endpoint.rx_closed.load(Ordering::Acquire)
                    || peer_endpoint.tx_closed.load(Ordering::Acquire))
                    && (self.endpoint.tx_closed.load(Ordering::Acquire)
                        || peer_endpoint.rx_closed.load(Ordering::Acquire))
            },
        );
        drop(channel);

        if is_listener_shutdown_changed && let Some(listener) = listener.as_ref() {
            listener.notify_shutdown();
        }
        if !is_rx_changed && !is_tx_changed {
            return Ok(());
        }

        if is_hung_up {
            self.endpoint.polls.wake_state_change();
            if let Some(peer_endpoint) = peer_endpoint.as_ref() {
                peer_endpoint.polls.wake_state_change();
            }
        } else {
            if is_rx_changed {
                self.endpoint.polls.readable.wake();
                if let Some(peer_endpoint) = peer_endpoint.as_ref() {
                    peer_endpoint.polls.writable.wake();
                }
            }
            if is_tx_changed {
                self.endpoint.polls.writable.wake();
                if let Some(peer_endpoint) = peer_endpoint.as_ref() {
                    peer_endpoint.polls.readable.wake();
                }
            }
        }
        Ok(())
    }
}

fn finish_send_on_error(total: usize, error: KError) -> KResult<usize> {
    if total > 0 { Ok(total) } else { Err(error) }
}

/// Delivers or disposes the reached record after the control guard is
/// released.
///
/// The record keeps the sender cmsg boundary: its entries are installed as
/// one `SCM_RIGHTS` cmsg of their own. A consuming receive without a control
/// buffer drops the reached record either way and reports the loss for
/// `MSG_CTRUNC` (`scm_detach_fds`), so a later `recvmsg` cannot inherit
/// stale control data. Both the sink merge (it allocates) and the disposal
/// (closing the last file reference may take a sleepable mutex) must stay
/// outside the guards.
fn deliver_reached(
    popped: Option<(u64, u64, Arc<Vec<AncillaryData>>)>,
    options: &mut RecvOptions<'_>,
) {
    match options.ancillary.as_deref_mut() {
        Some(sink) => popped
            .into_iter()
            .for_each(|(_, _, payload)| sink.extend(payload.iter().cloned())),
        None => {
            if popped.is_some()
                && let Some(discarded) = options.ancillary_discarded.as_deref_mut()
            {
                *discarded = true;
            }
            drop(popped);
        }
    }
}

/// Duplicates the record a peek touched, or reports the loss when the peek
/// cannot emit the entries. The record stays queued for the later consuming
/// receive either way.
fn deliver_peeked(touched: Option<Arc<Vec<AncillaryData>>>, options: &mut RecvOptions<'_>) {
    match options.ancillary.as_deref_mut() {
        Some(sink) => touched
            .into_iter()
            .for_each(|payload| sink.extend(payload.iter().cloned())),
        None => {
            if touched.is_some()
                && let Some(discarded) = options.ancillary_discarded.as_deref_mut()
            {
                *discarded = true;
            }
        }
    }
}

/// Registers `pending`'s payload for the byte range this round publishes, or
/// advances the write cursor for a plain-data round.
///
/// Called under the producer's `tx_order` lock with the round's `count`
/// known. On admission rejection the payload is returned to the caller,
/// which must drop it only after the guards are released: dropping the last
/// file reference can close a file that takes a sleepable mutex.
fn register_pending(
    chan: &mut Channel,
    pending: &mut Option<Arc<Vec<AncillaryData>>>,
    count: usize,
) -> Result<(), (channel::EnqueueError, Arc<Vec<AncillaryData>>)> {
    let mut anc = chan.tx_anc.lock();
    let start = anc.sent_total;
    let end = start + count as u64;
    match pending.take() {
        // Plain data round: advance the cursor only.
        None => {
            anc.sent_total = end;
            Ok(())
        }
        // Record the fd-bearing segment actually committed by this send:
        // [start, end). Reads stop at `end` (the SCM_RIGHTS barrier) even
        // when later plain-data sends leave no record boundary to infer it
        // from.
        Some(payload) => {
            let result = anc.enqueue_record(start, end, payload);
            if result.is_ok() {
                anc.sent_total = end;
            }
            result
        }
    }
}

impl Pollable for StreamTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some(chan) = self.channel.lock().as_ref() {
            let (is_rx_closed, is_peer_tx_closed, has_rx_data) = {
                let _tx_order = chan.peer_endpoint.tx_order.lock();
                (
                    self.endpoint.rx_closed.load(Ordering::Acquire),
                    chan.peer_endpoint.tx_closed.load(Ordering::Acquire)
                        || !chan.rx.write_is_held(),
                    chan.rx.occupied_len() > 0,
                )
            };
            let (is_tx_closed, is_peer_rx_closed, is_writable) = {
                let _tx_order = self.endpoint.tx_order.lock();
                (
                    self.endpoint.tx_closed.load(Ordering::Acquire),
                    chan.peer_endpoint.rx_closed.load(Ordering::Acquire) || !chan.tx.read_is_held(),
                    !chan.tx.read_is_held() || channel::is_stream_writable(chan.tx.occupied_len()),
                )
            };
            let is_receive_shutdown = is_rx_closed || is_peer_tx_closed;
            let is_send_shutdown = is_tx_closed || is_peer_rx_closed;
            events.set(IoEvents::IN, is_receive_shutdown || has_rx_data);
            events.set(IoEvents::OUT, is_writable);
            events.set(
                IoEvents::ERR,
                self.endpoint.socket_error.load(Ordering::Acquire) != 0,
            );
            events.set(IoEvents::RDHUP, is_receive_shutdown);
            events.set(IoEvents::HUP, is_receive_shutdown && is_send_shutdown);
        } else if let Some(listener) = self.listener.lock().as_ref() {
            let (is_listening, has_pending) = listener.poll_state();
            if is_listening {
                let is_rx_closed = self.endpoint.rx_closed.load(Ordering::Acquire);
                let is_tx_closed = self.endpoint.tx_closed.load(Ordering::Acquire);
                events.set(IoEvents::IN, is_rx_closed || has_pending);
                events.set(IoEvents::RDHUP, is_rx_closed);
                events.set(IoEvents::HUP, is_rx_closed && is_tx_closed);
            } else {
                events.insert(IoEvents::OUT | IoEvents::HUP);
            }
        } else {
            events.insert(IoEvents::OUT | IoEvents::HUP);
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.endpoint.polls.register(context, events)
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        let listener = self.listener.lock().take();
        if let Some(listener) = listener.as_ref() {
            listener.close();
        }
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        let channel = self.channel.lock().take();
        if let Some(channel) = channel {
            drop(channel);
        } else {
            self.endpoint.rx_closed.store(true, Ordering::Release);
            self.endpoint.tx_closed.store(true, Ordering::Release);
            self.endpoint.polls.wake_state_change();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/stream/tests.rs");
