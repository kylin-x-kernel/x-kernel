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

#[unittest::mod_test]
mod tests {
    use alloc::{
        sync::{Arc, Weak},
        task::Wake,
        vec,
        vec::Vec,
    };
    use core::{
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Waker},
    };

    use kerrno::{KError, KResult, LinuxError};
    use kio::{IoBufMut, Write};
    use kpoll::{IoEvents, PollRegistrations, PollSet, Pollable};
    use ksync::Mutex;
    use ktask::future::block_on;
    use ktime_types::TimeSpan;
    use ringbuf::traits::Observer;
    use unittest::{assert, assert_eq, def_test};

    use super::{
        StreamTransport, UnixTransportOps,
        channel::{
            MAX_PENDING_ANCILLARY, MAX_PENDING_RECORDS, STREAM_BUF_BYTES,
            STREAM_WRITABLE_MAX_OCCUPIED_BYTES,
        },
    };
    use crate::{
        AncillaryData, ConnectOptions, RecvFlags, RecvOptions, SendFlags, SendOptions, Shutdown,
        options::{Configurable, GetSocketOption, SetSocketOption},
        unix::{BindEntry, UnixAddr},
    };

    const TASK_WAIT_ROUNDS: usize = 100_000;

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    struct ChannelLockProbe {
        socket: Weak<StreamTransport>,
        was_unlocked: AtomicBool,
    }

    struct PausingWriter {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        byte: Option<u8>,
        has_paused: bool,
    }

    impl Write for PausingWriter {
        fn write(&mut self, buf: &[u8]) -> KResult<usize> {
            if !self.has_paused {
                self.has_paused = true;
                self.entered.store(true, Ordering::Release);
                while !self.release.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
            }
            let Some(byte) = buf.first() else {
                return Ok(0);
            };
            self.byte = Some(*byte);
            Ok(1)
        }

        fn flush(&mut self) -> KResult<()> {
            Ok(())
        }
    }

    impl IoBufMut for PausingWriter {
        fn remaining_mut(&self) -> usize {
            usize::from(self.byte.is_none())
        }
    }

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Wake for ChannelLockProbe {
        fn wake(self: Arc<Self>) {
            let was_unlocked = self
                .socket
                .upgrade()
                .is_some_and(|socket| socket.channel.try_lock().is_some());
            self.was_unlocked.store(was_unlocked, Ordering::SeqCst);
        }
    }

    struct Registration {
        counter: Arc<WakeCounter>,
        _registrations: PollRegistrations,
    }

    impl core::ops::Deref for Registration {
        type Target = WakeCounter;

        fn deref(&self) -> &Self::Target {
            &self.counter
        }
    }

    fn register(socket: &StreamTransport, events: IoEvents) -> Registration {
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        socket
            .register(&mut registrations.context(&context), events)
            .unwrap();
        Registration {
            counter,
            _registrations: registrations,
        }
    }

    fn register_channel_lock_probe(
        poll_set: &PollSet,
        socket: &Arc<StreamTransport>,
    ) -> (Arc<ChannelLockProbe>, PollRegistrations) {
        let probe = Arc::new(ChannelLockProbe {
            socket: Arc::downgrade(socket),
            was_unlocked: AtomicBool::new(false),
        });
        let waker = Waker::from(probe.clone());
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        registrations.context(&context).register(poll_set).unwrap();
        (probe, registrations)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        for _ in 0..TASK_WAIT_ROUNDS {
            if predicate() {
                return true;
            }
            ktask::yield_now();
        }
        false
    }

    #[def_test]
    fn unix_stream_data_flow_wakes_only_the_affected_direction() {
        let (left, right) = StreamTransport::new_pair(1);
        let left_read = register(&left, IoEvents::IN);
        let left_write = register(&left, IoEvents::OUT);
        let right_read = register(&right, IoEvents::IN);
        let right_read_second = register(&right, IoEvents::IN);
        let right_write = register(&right, IoEvents::OUT);
        let right_state = register(&right, IoEvents::HUP);

        assert_eq!(left.send(&b"x"[..], SendOptions::default()).unwrap(), 1);
        assert_eq!(left_read.0.load(Ordering::SeqCst), 0);
        assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
        assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
        assert_eq!(right_read_second.0.load(Ordering::SeqCst), 1);
        assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
        assert_eq!(right_state.0.load(Ordering::SeqCst), 0);

        let mut byte = [0_u8; 1];
        assert_eq!(
            right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
            1
        );
        assert_eq!(byte, [b'x']);
        assert_eq!(left_read.0.load(Ordering::SeqCst), 0);
        assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
        assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
        assert_eq!(right_state.0.load(Ordering::SeqCst), 0);
    }

    #[def_test]
    fn unix_stream_wakes_writers_only_after_crossing_the_low_watermark() {
        let (left, right) = StreamTransport::new_pair(1);
        let fill = vec![b'x'; STREAM_BUF_BYTES];
        assert_eq!(
            left.send(&fill[..], SendOptions::default()),
            Ok(STREAM_BUF_BYTES)
        );
        assert!(!left.poll().contains(IoEvents::OUT));

        let left_write = register(&left, IoEvents::OUT);
        let mut byte = [0_u8; 1];
        assert_eq!(right.recv(&mut byte[..], RecvOptions::default()), Ok(1));
        assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
        assert!(!left.poll().contains(IoEvents::OUT));

        let bytes_to_low_watermark = STREAM_BUF_BYTES - STREAM_WRITABLE_MAX_OCCUPIED_BYTES - 1;
        let mut data = vec![0_u8; bytes_to_low_watermark];
        assert_eq!(
            right.recv(&mut data[..], RecvOptions::default()),
            Ok(bytes_to_low_watermark)
        );
        assert_eq!(left_write.0.load(Ordering::SeqCst), 1);
        assert!(left.poll().contains(IoEvents::OUT));
    }

    #[def_test]
    fn unix_stream_nonblocking_send_returns_partial_or_would_block() {
        let (left, _right) = StreamTransport::new_pair(1);
        let fill = vec![b'x'; STREAM_BUF_BYTES - 1];

        assert_eq!(
            left.send(&fill[..], SendOptions::default()),
            Ok(STREAM_BUF_BYTES - 1)
        );

        let nonblocking = || SendOptions {
            flags: SendFlags::DONT_WAIT,
            ..SendOptions::default()
        };
        assert_eq!(left.send(&b"yz"[..], nonblocking()), Ok(1));
        assert!(matches!(
            left.send(&b"z"[..], nonblocking()),
            Err(KError::WouldBlock)
        ));
        assert_eq!(left.send(&b""[..], nonblocking()), Ok(0));
    }

    #[def_test]
    fn unix_stream_zero_length_recv_preserves_pending_data() {
        let (left, right) = StreamTransport::new_pair(1);
        let nonblocking = || RecvOptions {
            flags: RecvFlags::DONT_WAIT,
            ..RecvOptions::default()
        };
        let mut empty = [];

        assert_eq!(
            right.recv(&mut empty[..], nonblocking()),
            Err(KError::WouldBlock)
        );
        assert_eq!(left.send(&b"x"[..], SendOptions::default()), Ok(1));
        assert_eq!(right.recv(&mut empty[..], nonblocking()), Ok(0));

        let mut byte = [0_u8; 1];
        assert_eq!(right.recv(&mut byte[..], nonblocking()), Ok(1));
        assert_eq!(byte, [b'x']);
    }

    #[def_test]
    fn unix_stream_shutdown_write_wakes_peer_read_and_reports_eof() {
        let (left, right) = StreamTransport::new_pair(1);
        let right_read = register(&right, IoEvents::IN);
        let right_write = register(&right, IoEvents::OUT);
        let right_state = register(&right, IoEvents::RDHUP);

        left.shutdown(Shutdown::Write).unwrap();

        assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
        assert_eq!(right_write.0.load(Ordering::SeqCst), 0);
        assert_eq!(right_state.0.load(Ordering::SeqCst), 1);
        assert!(right.poll().contains(IoEvents::IN | IoEvents::RDHUP));

        let mut byte = [0_u8; 1];
        assert_eq!(
            right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
            0
        );
        assert!(matches!(
            left.send(&b"x"[..], SendOptions::default()),
            Err(KError::BrokenPipe)
        ));
    }

    #[def_test]
    fn unix_stream_hup_wakes_combined_direction_waiters() {
        let (left, right) = StreamTransport::new_pair(1);
        right.shutdown(Shutdown::Write).unwrap();
        assert!(!left.poll().contains(IoEvents::HUP));

        let read_or_hup = register(&left, IoEvents::IN | IoEvents::HUP);
        let hup = register(&left, IoEvents::HUP);
        left.shutdown(Shutdown::Write).unwrap();

        assert_eq!(read_or_hup.0.load(Ordering::SeqCst), 1);
        assert_eq!(hup.0.load(Ordering::SeqCst), 1);
        assert!(left.poll().contains(IoEvents::HUP));

        let (left, right) = StreamTransport::new_pair(1);
        left.shutdown(Shutdown::Write).unwrap();
        assert!(!left.poll().contains(IoEvents::HUP));

        let write_or_hup = register(&left, IoEvents::OUT | IoEvents::HUP);
        let hup = register(&left, IoEvents::HUP);
        right.shutdown(Shutdown::Write).unwrap();

        assert_eq!(write_or_hup.0.load(Ordering::SeqCst), 1);
        assert_eq!(hup.0.load(Ordering::SeqCst), 1);
        assert!(left.poll().contains(IoEvents::HUP));
    }

    #[def_test]
    fn unix_stream_shutdown_wakes_after_releasing_channel_lock() {
        let (left, right) = StreamTransport::new_pair(1);
        let left = Arc::new(left);
        let (local_wake_saw_unlocked, _local_registration) =
            register_channel_lock_probe(&left.endpoint.polls.writable, &left);
        let (peer_wake_saw_unlocked, _peer_registration) =
            register_channel_lock_probe(&right.endpoint.polls.readable, &left);

        left.shutdown(Shutdown::Write).unwrap();

        assert!(local_wake_saw_unlocked.was_unlocked.load(Ordering::SeqCst));
        assert!(peer_wake_saw_unlocked.was_unlocked.load(Ordering::SeqCst));
    }

    #[def_test]
    fn unix_stream_shutdown_read_drains_buffer_before_eof() {
        let (left, right) = StreamTransport::new_pair(1);

        assert_eq!(left.send(&b"abc"[..], SendOptions::default()).unwrap(), 3);
        right.shutdown(Shutdown::Read).unwrap();

        let mut data = [0_u8; 3];
        assert_eq!(
            right.recv(&mut data[..], RecvOptions::default()).unwrap(),
            3
        );
        assert_eq!(data, *b"abc");
        assert_eq!(
            right.recv(&mut data[..], RecvOptions::default()).unwrap(),
            0
        );
        assert!(matches!(
            left.send(&b"x"[..], SendOptions::default()),
            Err(KError::BrokenPipe)
        ));
    }

    #[def_test(serial)]
    fn unix_stream_eof_waits_for_data_published_after_empty_snapshot() {
        let (left, right) = StreamTransport::new_pair(1);
        let left = Arc::new(left);
        let right = Arc::new(right);
        let recv_entered = Arc::new(AtomicBool::new(false));
        let recv_release = Arc::new(AtomicBool::new(false));
        let recv_result = Arc::new(Mutex::new(None));

        let recv_task = ktask::spawn({
            let right = right.clone();
            let recv_entered = recv_entered.clone();
            let recv_release = recv_release.clone();
            let recv_result = recv_result.clone();
            move || {
                let mut writer = PausingWriter {
                    entered: recv_entered,
                    release: recv_release,
                    byte: None,
                    has_paused: false,
                };
                let result = right.recv(&mut writer, RecvOptions::default());
                *recv_result.lock() = Some((result, writer.byte));
            }
        });
        let recv_is_paused = wait_until(|| recv_entered.load(Ordering::Acquire));
        if !recv_is_paused {
            recv_release.store(true, Ordering::Release);
            left.shutdown(Shutdown::Write).unwrap();
            recv_task.join();
        }
        assert!(recv_is_paused);

        let send_result = Arc::new(Mutex::new(None));
        let send_task = ktask::spawn({
            let left = left.clone();
            let send_result = send_result.clone();
            move || {
                *send_result.lock() = Some(left.send(&b"x"[..], SendOptions::default()));
            }
        });
        send_task.join();

        let shutdown_result = Arc::new(Mutex::new(None));
        let shutdown_task = ktask::spawn({
            let left = left.clone();
            let shutdown_result = shutdown_result.clone();
            move || {
                *shutdown_result.lock() = Some(left.shutdown(Shutdown::Write));
            }
        });
        shutdown_task.join();

        recv_release.store(true, Ordering::Release);
        recv_task.join();

        assert_eq!(send_result.lock().take().unwrap(), Ok(1));
        assert_eq!(shutdown_result.lock().take().unwrap(), Ok(()));
        let (result, byte) = recv_result.lock().take().unwrap();
        assert_eq!(result, Ok(1));
        assert_eq!(byte, Some(b'x'));

        let mut byte = [0_u8; 1];
        assert_eq!(
            right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
            0
        );
    }

    #[def_test(serial)]
    fn unix_stream_shutdown_write_preserves_partial_send() {
        let (left, right) = StreamTransport::new_pair(1);
        let left = Arc::new(left);
        let right = Arc::new(right);
        let send_result = Arc::new(Mutex::new(None));

        let send_task = ktask::spawn({
            let left = left.clone();
            let send_result = send_result.clone();
            move || {
                let data = vec![b'x'; STREAM_BUF_BYTES + 1];
                *send_result.lock() = Some(left.send(&data[..], SendOptions::default()));
            }
        });
        let buffer_is_full = wait_until(|| {
            right
                .channel
                .lock()
                .as_ref()
                .is_some_and(|channel| channel.rx.occupied_len() == STREAM_BUF_BYTES)
        });
        if !buffer_is_full {
            left.shutdown(Shutdown::Write).unwrap();
            send_task.join();
        }
        assert!(buffer_is_full);

        left.shutdown(Shutdown::Write).unwrap();
        send_task.join();
        assert_eq!(send_result.lock().take().unwrap(), Ok(STREAM_BUF_BYTES));

        let mut data: Vec<u8> = vec![0; STREAM_BUF_BYTES];
        assert_eq!(
            right.recv(&mut data[..], RecvOptions::default()).unwrap(),
            STREAM_BUF_BYTES
        );
        assert!(data.iter().all(|byte| *byte == b'x'));
        assert_eq!(
            right.recv(&mut data[..1], RecvOptions::default()).unwrap(),
            0
        );
    }

    #[def_test]
    fn unix_stream_shutdown_read_does_not_wake_local_write_waiters() {
        let (left, _right) = StreamTransport::new_pair(1);
        let left_read = register(&left, IoEvents::IN);
        let left_write = register(&left, IoEvents::OUT);

        left.shutdown(Shutdown::Read).unwrap();

        assert_eq!(left_read.0.load(Ordering::SeqCst), 1);
        assert_eq!(left_write.0.load(Ordering::SeqCst), 0);
    }

    #[def_test]
    fn unix_stream_peer_drop_wakes_connection_waiters_once() {
        let (left, right) = StreamTransport::new_pair(1);
        let right_read = register(&right, IoEvents::IN);
        let right_write = register(&right, IoEvents::OUT);

        drop(left);

        assert_eq!(right_read.0.load(Ordering::SeqCst), 1);
        assert_eq!(right_write.0.load(Ordering::SeqCst), 1);
        assert!(
            right
                .poll()
                .contains(IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP)
        );

        let mut byte = [0_u8; 1];
        assert_eq!(
            right.recv(&mut byte[..], RecvOptions::default()).unwrap(),
            0
        );
        assert!(matches!(
            right.send(&b"x"[..], SendOptions::default()),
            Err(KError::BrokenPipe)
        ));
    }

    #[def_test]
    fn unix_stream_peer_drop_with_unread_input_reports_reset_once() {
        let (left, right) = StreamTransport::new_pair(1);
        assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
        assert_eq!(right.send(&b"r"[..], SendOptions::default()), Ok(1));
        drop(right);

        assert!(left.poll().contains(IoEvents::ERR));
        let mut byte = [0_u8; 1];
        assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(1));
        assert_eq!(byte, [b'r']);
        assert_eq!(
            left.recv(&mut byte[..], RecvOptions::default()),
            Err(KError::ConnectionReset)
        );
        assert!(!left.poll().contains(IoEvents::ERR));
        assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(0));

        let (left, right) = StreamTransport::new_pair(1);
        assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
        drop(right);
        let mut error = 0;
        left.get_option(GetSocketOption::Error(&mut error)).unwrap();
        assert_eq!(error, LinuxError::ECONNRESET.into_raw());
        left.get_option(GetSocketOption::Error(&mut error)).unwrap();
        assert_eq!(error, 0);
        assert_eq!(left.recv(&mut byte[..], RecvOptions::default()), Ok(0));
    }

    #[def_test]
    fn unix_stream_connection_changes_rearm_existing_waiters() {
        let listener = StreamTransport::new(1);
        let client = StreamTransport::new(2);
        let entry = BindEntry::default();
        let listener_read = register(&listener, IoEvents::IN);

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);
        listener.listen(1).unwrap();

        let client_read = register(&client, IoEvents::IN);
        client
            .connect(&entry, &UnixAddr::Unbound, ConnectOptions::default())
            .unwrap();
        assert_eq!(client_read.0.load(Ordering::SeqCst), 1);
    }

    #[def_test]
    fn unix_stream_backlog_limits_pending_connections() {
        let listener = StreamTransport::new(1);
        let first = StreamTransport::new(2);
        let second = StreamTransport::new(3);
        let third = StreamTransport::new(4);
        let entry = BindEntry::default();
        let nonblocking = ConnectOptions { nonblocking: true };

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1).unwrap();
        first
            .connect(&entry, &UnixAddr::Unbound, nonblocking)
            .unwrap();
        assert_eq!(
            first.connect(&entry, &UnixAddr::Unbound, nonblocking),
            Err(KError::AlreadyConnected)
        );
        second
            .connect(&entry, &UnixAddr::Unbound, nonblocking)
            .unwrap();
        assert_eq!(
            third.connect(&entry, &UnixAddr::Unbound, nonblocking),
            Err(KError::WouldBlock)
        );

        let accepted = block_on(listener.accept(true)).unwrap();
        third
            .connect(&entry, &UnixAddr::Unbound, nonblocking)
            .unwrap();
        drop(accepted);
    }

    #[def_test]
    fn unix_stream_connect_honors_send_timeout_when_backlog_is_full() {
        let listener = StreamTransport::new(1);
        let queued = StreamTransport::new(2);
        let waiting = StreamTransport::new(3);
        let entry = BindEntry::default();

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(0).unwrap();
        queued
            .connect(&entry, &UnixAddr::Unbound, ConnectOptions::default())
            .unwrap();

        let send_timeout = TimeSpan::from_nanos(1);
        waiting
            .set_option(SetSocketOption::SendTimeout(&send_timeout))
            .unwrap();
        assert_eq!(
            waiting.connect(&entry, &UnixAddr::Unbound, ConnectOptions::default()),
            Err(KError::WouldBlock)
        );
    }

    #[def_test(serial)]
    fn unix_stream_accept_releases_blocked_connector() {
        let listener = Arc::new(StreamTransport::new(1));
        let queued_client = StreamTransport::new(2);
        let waiting_client = Arc::new(StreamTransport::new(3));
        let entry = Arc::new(BindEntry::default());

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(0).unwrap();
        queued_client
            .connect(&entry, &UnixAddr::Unbound, ConnectOptions::default())
            .unwrap();

        let connect_result = Arc::new(Mutex::new(None));
        let connect_task = ktask::spawn({
            let waiting_client = waiting_client.clone();
            let entry = entry.clone();
            let connect_result = connect_result.clone();
            move || {
                *connect_result.lock() = Some(waiting_client.connect(
                    &entry,
                    &UnixAddr::Unbound,
                    ConnectOptions::default(),
                ));
            }
        });
        let connector_is_waiting = wait_until(|| connect_task.state() == ktask::TaskState::Blocked);
        if !connector_is_waiting {
            listener.shutdown(Shutdown::Read).unwrap();
            connect_task.join();
        }
        assert!(connector_is_waiting);

        let first_accepted = block_on(listener.accept(false)).unwrap();
        connect_task.join();
        assert_eq!(connect_result.lock().take().unwrap(), Ok(()));
        let second_accepted = block_on(listener.accept(true)).unwrap();
        drop((first_accepted, second_accepted));
    }

    #[def_test]
    fn unix_stream_repeated_listen_updates_backlog() {
        let listener = StreamTransport::new(1);
        let first = StreamTransport::new(2);
        let second = StreamTransport::new(3);
        let entry = BindEntry::default();
        let nonblocking = ConnectOptions { nonblocking: true };

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(0).unwrap();
        first
            .connect(&entry, &UnixAddr::Unbound, nonblocking)
            .unwrap();
        assert_eq!(
            second.connect(&entry, &UnixAddr::Unbound, nonblocking),
            Err(KError::WouldBlock)
        );

        listener.listen(1).unwrap();
        second
            .connect(&entry, &UnixAddr::Unbound, nonblocking)
            .unwrap();
    }

    #[def_test]
    fn unix_stream_listening_socket_cannot_connect() {
        let listener = StreamTransport::new(1);
        let entry = BindEntry::default();

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1).unwrap();
        assert_eq!(
            listener.connect(&entry, &UnixAddr::Unbound, ConnectOptions::default()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            block_on(listener.accept(true)).map(|_| ()),
            Err(KError::WouldBlock)
        );
    }

    #[def_test]
    fn unix_stream_stale_listener_returns_connection_refused() {
        let entry = BindEntry::default();
        {
            let listener = StreamTransport::new(1);
            listener.bind(&entry, &UnixAddr::Unbound).unwrap();
            listener.listen(1).unwrap();
        }

        let client = StreamTransport::new(2);
        assert_eq!(
            client.connect(&entry, &UnixAddr::Unbound, ConnectOptions::default()),
            Err(KError::ConnectionRefused)
        );
    }

    #[def_test(serial)]
    fn unix_stream_listener_shutdown_wakes_connect_and_rejects_empty_accept() {
        let listener = Arc::new(StreamTransport::new(1));
        let queued_client = StreamTransport::new(2);
        let waiting_client = Arc::new(StreamTransport::new(3));
        let entry = Arc::new(BindEntry::default());

        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(0).unwrap();
        queued_client
            .connect(&entry, &UnixAddr::Unbound, ConnectOptions::default())
            .unwrap();

        let connect_result = Arc::new(Mutex::new(None));
        let connect_task = ktask::spawn({
            let waiting_client = waiting_client.clone();
            let entry = entry.clone();
            let connect_result = connect_result.clone();
            move || {
                *connect_result.lock() = Some(waiting_client.connect(
                    &entry,
                    &UnixAddr::Unbound,
                    ConnectOptions::default(),
                ));
            }
        });
        let connector_is_waiting = wait_until(|| connect_task.state() == ktask::TaskState::Blocked);
        if !connector_is_waiting {
            listener.shutdown(Shutdown::Read).unwrap();
            connect_task.join();
        }
        assert!(connector_is_waiting);

        listener.shutdown(Shutdown::Read).unwrap();
        connect_task.join();
        assert_eq!(
            connect_result.lock().take().unwrap(),
            Err(KError::ConnectionRefused)
        );

        let accepted = block_on(listener.accept(false)).unwrap();
        drop(accepted);
        assert_eq!(
            block_on(listener.accept(false)).map(|_| ()),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            block_on(listener.accept(true)).map(|_| ()),
            Err(KError::WouldBlock)
        );
    }

    #[def_test(serial)]
    fn unix_stream_listener_shutdown_wakes_blocked_accept() {
        let listener = Arc::new(StreamTransport::new(1));
        let entry = BindEntry::default();
        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1).unwrap();

        let accept_result = Arc::new(Mutex::new(None));
        let accept_task = ktask::spawn({
            let listener = listener.clone();
            let accept_result = accept_result.clone();
            move || {
                *accept_result.lock() = Some(block_on(listener.accept(false)).map(|_| ()));
            }
        });
        let acceptor_is_waiting = wait_until(|| accept_task.state() == ktask::TaskState::Blocked);
        if !acceptor_is_waiting {
            listener.shutdown(Shutdown::Read).unwrap();
            accept_task.join();
        }
        assert!(acceptor_is_waiting);

        listener.shutdown(Shutdown::Read).unwrap();
        accept_task.join();
        assert_eq!(
            accept_result.lock().take().unwrap(),
            Err(KError::InvalidInput)
        );
    }

    #[def_test]
    fn unix_stream_listener_does_not_leave_a_stale_read_registration() {
        let listener = StreamTransport::new(1);
        let client = StreamTransport::new(2);
        let entry = BindEntry::default();
        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1).unwrap();
        let listener_read = register(&listener, IoEvents::IN);

        client
            .connect(&entry, &UnixAddr::Unbound, ConnectOptions::default())
            .unwrap();
        assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);

        listener.endpoint.polls.readable.wake();
        assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);
    }

    #[def_test]
    fn unix_stream_delivers_ancillary_once_with_sink() {
        let (sender, receiver) = StreamTransport::new_pair(7);
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();
        let send_options = SendOptions {
            ancillary: vec![Arc::new(7_u32) as AncillaryData, Arc::new(8_u32)],
            ..SendOptions::default()
        };
        sender.send(&b"xy"[..], send_options).unwrap();

        let mut sink: Vec<AncillaryData> = Vec::new();
        let mut buf = [0_u8; 2];
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(buf, *b"xy");
        // every fd of the consumed range arrives exactly once
        assert_eq!(sink.len(), 2);
        assert_eq!(sink[0].downcast_ref::<u32>(), Some(&7));
        assert_eq!(sink[1].downcast_ref::<u32>(), Some(&8));

        // no duplicate delivery on the next consume
        let mut sink2: Vec<AncillaryData> = Vec::new();
        let r = receiver.recv(
            &mut buf[..],
            RecvOptions {
                ancillary: Some(&mut sink2),
                ..RecvOptions::default()
            },
        );
        assert!(matches!(r, Err(KError::WouldBlock)));
        assert!(sink2.is_empty());
    }

    #[def_test]
    fn unix_stream_drops_unsunk_ancillary_without_late_delivery() {
        let (sender, receiver) = StreamTransport::new_pair(7);
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();
        let send_options = SendOptions {
            ancillary: vec![Arc::new(9_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"a"[..], send_options).unwrap();

        let mut buf = [0_u8; 1];
        let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'a');

        // a later sink-consuming recv must not see the dropped record
        let send_options = SendOptions {
            ancillary: vec![Arc::new(10_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"b"[..], send_options).unwrap();
        let mut sink: Vec<AncillaryData> = Vec::new();
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'b');
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].downcast_ref::<u32>(), Some(&10));
    }

    #[def_test]
    fn unix_stream_receiver_close_releases_unsunk_records() {
        // Drop-observable ancillary payload: if the queued record survives
        // the receiver's close (the pre-take_all behaviour, or a reference
        // cycle pinning the file), the flag stays unset.
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let (sender, receiver) = StreamTransport::new_pair(7);
        let flag = Arc::new(AtomicBool::new(false));
        let send_options = SendOptions {
            ancillary: vec![Arc::new(DropFlag(flag.clone())) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"a"[..], send_options).unwrap();

        drop(receiver);

        assert!(
            flag.load(Ordering::SeqCst),
            "queued record must be released when the receiver closes"
        );
    }

    #[def_test]
    fn unix_stream_recv_stops_at_ancillary_barrier() {
        let (sender, receiver) = StreamTransport::new_pair(7);
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        // two sendmsg boundaries, each carrying its own record
        let so1 = SendOptions {
            ancillary: vec![Arc::new(1_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"a"[..], so1).unwrap();
        let so2 = SendOptions {
            ancillary: vec![Arc::new(2_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"bb"[..], so2).unwrap();

        // one large recv: Linux stops this read at the first fd segment
        let mut sink: Vec<AncillaryData> = Vec::new();
        let mut buf = [0_u8; 16];
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'a');
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].downcast_ref::<u32>(), Some(&1));

        // the second segment and its record wait for the next recv
        let mut sink2: Vec<AncillaryData> = Vec::new();
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink2),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(&buf[..2], b"bb");
        assert_eq!(sink2.len(), 1);
        assert_eq!(sink2[0].downcast_ref::<u32>(), Some(&2));
    }

    #[def_test]
    fn unix_stream_wrapped_recv_has_no_duplicate_bytes() {
        // Regression for the duplicated second-slice write: fill the ring
        // so the readable region wraps across the buffer end, then drain
        // it with a buffer larger than the occupied bytes. Pre-fix, the
        // tail slice was written twice (duplicate bytes, over-advanced
        // cursor); this test fails on that behaviour.
        const RING: usize = STREAM_BUF_BYTES;
        assert_eq!(RING, 65_536);
        let (sender, receiver) = StreamTransport::new_pair(7);
        let sent: Vec<u8> = (0..125_536_u32).map(|i| (i % 251) as u8).collect();

        sender
            .send(&sent[..65_000], SendOptions::default())
            .unwrap();
        let mut buf = vec![0_u8; 60_000];
        let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
        assert_eq!(n, 60_000);
        assert_eq!(&buf[..60_000], &sent[..60_000]);

        // free space is now 60_536: this send wraps the write index past
        // the buffer end
        let n = sender
            .send(&sent[65_000..], SendOptions::default())
            .unwrap();
        assert_eq!(n, 60_536);

        let mut buf = vec![0_u8; 70_000];
        let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
        assert_eq!(n, 65_536);
        // the wrapped tail appears exactly once
        assert_eq!(&buf[..5_536], &sent[60_000..65_536]);
        let cont: Vec<u8> = (65_536_u32..125_536).map(|i| (i % 251) as u8).collect();
        assert_eq!(&buf[5_536..65_536], cont.as_slice());
    }

    #[def_test]
    fn unix_stream_barrier_stops_before_plain_data_between_records() {
        // Review case: A+fd1 -> plain MID -> B+fd2. Linux returns "A"+fd1
        // first (the fd segment ends with its own sendmsg; the plain MID
        // in between does NOT extend it), then "MIDB"+fd2.
        let (sender, receiver) = StreamTransport::new_pair(7);
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        let so1 = SendOptions {
            ancillary: vec![Arc::new(1_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"A"[..], so1).unwrap();
        sender.send(&b"MID"[..], SendOptions::default()).unwrap();
        let so2 = SendOptions {
            ancillary: vec![Arc::new(2_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"B"[..], so2).unwrap();

        let mut sink: Vec<AncillaryData> = Vec::new();
        let mut buf = [0_u8; 16];
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'A');
        assert_eq!(sink.len(), 1);
        assert_eq!(sink[0].downcast_ref::<u32>(), Some(&1));

        // second recv: plain MID followed by B with its fd
        let mut sink2: Vec<AncillaryData> = Vec::new();
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink2),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..4], b"MIDB");
        assert_eq!(sink2.len(), 1);
        assert_eq!(sink2[0].downcast_ref::<u32>(), Some(&2));
    }

    #[def_test]
    fn unix_stream_barrier_covers_fd_segment_followed_by_plain_data() {
        // Review case: plain PRE -> A+fd1 -> plain POST. Linux returns
        // "PREA"+fd1 first (prefix plus the fd segment), leaving "POST".
        let (sender, receiver) = StreamTransport::new_pair(7);
        receiver
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        sender.send(&b"PRE"[..], SendOptions::default()).unwrap();
        let so = SendOptions {
            ancillary: vec![Arc::new(7_u32) as AncillaryData],
            ..SendOptions::default()
        };
        sender.send(&b"A"[..], so).unwrap();
        sender.send(&b"POST"[..], SendOptions::default()).unwrap();

        let mut sink: Vec<AncillaryData> = Vec::new();
        let mut buf = [0_u8; 16];
        let n = receiver
            .recv(
                &mut buf[..],
                RecvOptions {
                    ancillary: Some(&mut sink),
                    ..RecvOptions::default()
                },
            )
            .unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..4], b"PREA");
        assert_eq!(sink.len(), 1);

        let n = receiver.recv(&mut buf[..], RecvOptions::default()).unwrap();
        assert_eq!(n, 4);
        assert_eq!(&buf[..4], b"POST");
    }

    #[def_test]
    fn unix_stream_recv_without_control_buffer_reports_discard() {
        let (left, right) = StreamTransport::new_pair(1);

        assert_eq!(
            left.send(
                &b"abc"[..],
                SendOptions {
                    ancillary: vec![Arc::new(9_u32) as AncillaryData],
                    ..SendOptions::default()
                }
            ),
            Ok(3)
        );

        // Consuming the record's segment without a control buffer drops the
        // record and reports the loss (MSG_CTRUNC) to the caller.
        let nonblocking = RecvFlags::DONT_WAIT;
        let mut data = [0_u8; 1];
        let mut discarded = false;
        let recv = right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: None,
                ancillary_discarded: Some(&mut discarded),
                ..RecvOptions::default()
            },
        );
        assert_eq!(recv, Ok(1));
        assert!(discarded);

        // Plain data afterwards reaches no record, so the flag stays clear.
        let mut data = [0_u8; 2];
        let mut discarded = false;
        let recv = right.recv(
            &mut data[..],
            RecvOptions {
                flags: nonblocking,
                ancillary: None,
                ancillary_discarded: Some(&mut discarded),
                ..RecvOptions::default()
            },
        );
        assert_eq!(recv, Ok(2));
        assert!(!discarded);
    }

    #[def_test]
    fn unix_stream_peek_duplicates_control_data_and_preserves_data() {
        let (left, right) = StreamTransport::new_pair(1);
        right
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        assert_eq!(left.send(&b"abc"[..], SendOptions::default()), Ok(3));
        assert_eq!(
            left.send(
                &b"def"[..],
                SendOptions {
                    ancillary: vec![control(7)],
                    ..SendOptions::default()
                }
            ),
            Ok(3)
        );

        // The peek window reaches the record's segment: it duplicates the
        // shared entry references without consuming the record or the data,
        // and stops at the segment end like a consuming recv.
        let peek = RecvFlags::PEEK | RecvFlags::DONT_WAIT;
        let mut data = [0_u8; 6];
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: peek,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(6)
        );
        assert_eq!(&data[..6], b"abcdef");
        assert_eq!(received_controls(&ancillary), vec![7]);

        // A second peek duplicates the same control data again.
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: peek,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(6)
        );
        assert_eq!(received_controls(&ancillary), vec![7]);

        // A consuming recv still receives both the data and the record.
        let nonblocking = RecvFlags::DONT_WAIT;
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: nonblocking,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(6)
        );
        assert_eq!(&data[..6], b"abcdef");
        assert_eq!(received_controls(&ancillary), vec![7]);

        // The record is retired after the consuming recv; the stream is then
        // empty.
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: nonblocking,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Err(KError::WouldBlock)
        );
        assert!(received_controls(&ancillary).is_empty());
    }

    #[def_test]
    fn unix_stream_peek_without_control_buffer_reports_discard_and_keeps_batch() {
        let (left, right) = StreamTransport::new_pair(1);
        right
            .set_option(SetSocketOption::NonBlocking(&true))
            .unwrap();

        assert_eq!(
            left.send(
                &b"abc"[..],
                SendOptions {
                    ancillary: vec![control(9)],
                    ..SendOptions::default()
                }
            ),
            Ok(3)
        );

        // A peek without a control buffer cannot emit the reached entries
        // either, so the loss is reported even though the peek keeps the
        // record queued for the consuming receive. A one-byte buffer keeps
        // the partial-touch path: reaching only the record's first byte
        // already duplicates -- and here reports -- the record.
        let peek = RecvFlags::PEEK | RecvFlags::DONT_WAIT;
        let mut data = [0_u8; 1];
        let mut discarded = false;
        let recv = right.recv(
            &mut data[..],
            RecvOptions {
                flags: peek,
                ancillary: None,
                ancillary_discarded: Some(&mut discarded),
                ..RecvOptions::default()
            },
        );
        assert_eq!(recv, Ok(1));
        assert_eq!(data[0], b'a');
        assert!(discarded);

        // The record is still delivered by the later consuming receive.
        let nonblocking = RecvFlags::DONT_WAIT;
        let mut data = [0_u8; 3];
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: nonblocking,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(3)
        );
        assert_eq!(&data[..3], b"abc");
        assert_eq!(received_controls(&ancillary), vec![9]);
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: nonblocking,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Err(KError::WouldBlock)
        );
    }

    /// Opaque control payload for the quota tests. The transport treats
    /// `AncillaryData` as type-erased; tests downcast to verify that a record
    /// was delivered rather than dropped or duplicated.
    #[derive(Debug, PartialEq)]
    struct TestControl(u32);

    fn control(id: u32) -> AncillaryData {
        Arc::new(TestControl(id)) as AncillaryData
    }

    fn received_controls(ancillary: &[AncillaryData]) -> Vec<u32> {
        ancillary
            .iter()
            .filter_map(|item| item.downcast_ref::<TestControl>().map(|control| control.0))
            .collect()
    }

    #[def_test]
    fn unix_stream_pending_record_quota_rejects_sends_until_drained() {
        let (left, right) = StreamTransport::new_pair(1);
        let one_fd = |id: u32| SendOptions {
            ancillary: vec![control(id)],
            ..SendOptions::default()
        };
        let enobufs = KError::from(LinuxError::ENOBUFS);

        // One-byte sends each pin one retained record; the quota accepts
        // exactly MAX_PENDING_RECORDS of them, then fails the send.
        for id in 0..MAX_PENDING_RECORDS as u32 {
            assert_eq!(left.send(&b"x"[..], one_fd(id)), Ok(1));
        }
        assert_eq!(left.send(&b"x"[..], one_fd(u32::MAX)), Err(enobufs));

        // Plain data is not subject to the record quota.
        assert_eq!(left.send(&b"yy"[..], SendOptions::default()), Ok(2));

        // Retiring one record frees its retention; the next cmsg send
        // succeeds again.
        let mut data = [0_u8; 1];
        let mut ancillary = Vec::new();
        assert_eq!(
            right.recv(
                &mut data[..],
                RecvOptions {
                    flags: RecvFlags::DONT_WAIT,
                    ancillary: Some(&mut ancillary),
                    ..RecvOptions::default()
                },
            ),
            Ok(1)
        );
        assert_eq!(received_controls(&ancillary), vec![0]);
        assert_eq!(left.send(&b"x"[..], one_fd(999)), Ok(1));
    }

    #[def_test]
    fn unix_stream_oversized_control_batch_rejects_with_enobufs() {
        let (left, _right) = StreamTransport::new_pair(1);

        // A single message whose entries exceed the retention bound is
        // rejected even on an empty queue; each entry is one control message
        // whose descriptor count `posix-net` caps at `SCM_MAX_FD` (253).
        let oversized = SendOptions {
            ancillary: (0..=MAX_PENDING_ANCILLARY as u32).map(control).collect(),
            ..SendOptions::default()
        };
        assert_eq!(
            left.send(&b"x"[..], oversized),
            Err(KError::from(LinuxError::ENOBUFS))
        );
    }

    #[def_test]
    fn unix_stream_blocking_send_registers_its_record_once() {
        let (left, right) = StreamTransport::new_pair(1);
        let left = Arc::new(left);
        let send_result = Arc::new(Mutex::new(None));
        // Larger than the ring buffer, so the blocking send is replayed over
        // several `poll_io` rounds and must still register one record.
        let payload = vec![b'x'; STREAM_BUF_BYTES + 16];

        let send_task = ktask::spawn({
            let left = left.clone();
            let send_result = send_result.clone();
            move || {
                let options = SendOptions {
                    ancillary: vec![control(7)],
                    ..SendOptions::default()
                };
                let result = left.send(&payload[..], options);
                // Close the write side so an early send failure cannot leave
                // the reader blocked.
                left.shutdown(Shutdown::Write).unwrap();
                *send_result.lock() = Some(result);
            }
        });

        let mut buf = vec![0_u8; 4096];
        let mut received = 0;
        let mut first_controls = None;
        let mut later_controls = Vec::new();
        loop {
            let mut ancillary = Vec::new();
            let count = right
                .recv(
                    &mut buf[..],
                    RecvOptions {
                        ancillary: Some(&mut ancillary),
                        ..RecvOptions::default()
                    },
                )
                .unwrap();
            if count == 0 {
                break;
            }
            if first_controls.is_none() {
                first_controls = Some(received_controls(&ancillary));
            } else {
                later_controls.extend(received_controls(&ancillary));
            }
            received += count;
        }
        send_task.join();

        assert_eq!(
            send_result.lock().take().unwrap(),
            Ok(STREAM_BUF_BYTES + 16)
        );
        assert_eq!(received, STREAM_BUF_BYTES + 16);
        // The one record is delivered with the first recv that reaches the
        // send's first byte, even though the send was replayed over several
        // poll rounds.
        assert_eq!(first_controls, Some(vec![7]));
        assert!(later_controls.is_empty());
    }
}
