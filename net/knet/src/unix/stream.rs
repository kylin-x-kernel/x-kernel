// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix stream socket transport.
use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
    task::Context,
};

use async_trait::async_trait;
use kerrno::{KError, KResult, LinuxError};
use kio::{IoBuf, Read, Write};
use kpoll::{IoEvents, PollSet, Pollable};
use kspin::SpinNoPreempt;
use ksync::Mutex;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::{
    RecvOptions, SendOptions, Shutdown,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

const STREAM_BUF_BYTES: usize = 64 * 1024;
const STREAM_WRITABLE_MAX_OCCUPIED_BYTES: usize = STREAM_BUF_BYTES / 4;

fn is_stream_writable(occupied_bytes: usize) -> bool {
    occupied_bytes <= STREAM_WRITABLE_MAX_OCCUPIED_BYTES
}

fn new_ring_pair() -> (HeapProd<u8>, HeapCons<u8>) {
    let rb = HeapRb::new(STREAM_BUF_BYTES);
    rb.split()
}

fn finish_send_on_error(total: usize, error: KError) -> KResult<usize> {
    if total > 0 { Ok(total) } else { Err(error) }
}

#[derive(Default)]
struct StreamPollSets {
    readable: PollSet,
    writable: PollSet,
    state: PollSet,
}

impl StreamPollSets {
    fn register(&self, waker: &core::task::Waker, events: IoEvents) {
        let has_read_events =
            events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND | IoEvents::RDHUP);
        let has_write_events =
            events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND);
        if has_read_events {
            self.readable.register(waker);
        }
        if has_write_events {
            self.writable.register(waker);
        }
        if !has_read_events && !has_write_events && events.intersects(IoEvents::ERR | IoEvents::HUP)
        {
            self.state.register(waker);
        }
    }

    fn wake_state_change(&self) {
        self.readable.wake();
        self.writable.wake();
        self.state.wake();
    }
}

#[derive(Default)]
struct StreamEndpoint {
    polls: StreamPollSets,
    /// Orders data publication, shutdown, and EOF observation for this
    /// endpoint's transmit direction.
    tx_order: SpinNoPreempt<()>,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
    is_listening: AtomicBool,
    socket_error: AtomicI32,
}

fn new_duplex_channel(
    client_endpoint: Arc<StreamEndpoint>,
    server_endpoint: Arc<StreamEndpoint>,
    pid: u32,
) -> (Channel, Channel) {
    let (client_tx, server_rx) = new_ring_pair();
    let (server_tx, client_rx) = new_ring_pair();
    (
        Channel {
            tx: client_tx,
            rx: client_rx,
            endpoint: client_endpoint.clone(),
            peer_endpoint: server_endpoint.clone(),
            peer_pid: pid,
        },
        Channel {
            tx: server_tx,
            rx: server_rx,
            endpoint: server_endpoint,
            peer_endpoint: client_endpoint,
            peer_pid: pid,
        },
    )
}

struct Channel {
    tx: HeapProd<u8>,
    rx: HeapCons<u8>,
    endpoint: Arc<StreamEndpoint>,
    peer_endpoint: Arc<StreamEndpoint>,
    peer_pid: u32,
}

impl Drop for Channel {
    fn drop(&mut self) {
        let (is_rx_changed, has_unread_input) = {
            let _tx_order = self.peer_endpoint.tx_order.lock();
            (
                !self.endpoint.rx_closed.swap(true, Ordering::AcqRel),
                self.rx.occupied_len() > 0,
            )
        };
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

pub struct Bind {
    /// New connections are sent to this channel.
    accept_tx: async_channel::Sender<ConnRequest>,
    listener_endpoint: Arc<StreamEndpoint>,
    pid: u32,
}
impl Bind {
    fn connect(
        &self,
        client_endpoint: Arc<StreamEndpoint>,
        local_addr: UnixAddr,
        pid: u32,
    ) -> KResult<Channel> {
        if !self.listener_endpoint.is_listening.load(Ordering::Acquire)
            || self.listener_endpoint.rx_closed.load(Ordering::Acquire)
        {
            return Err(KError::ConnectionRefused);
        }
        let server_endpoint = Arc::new(StreamEndpoint::default());
        let (mut client_chan, mut server_chan) =
            new_duplex_channel(client_endpoint, server_endpoint, 0);
        client_chan.peer_pid = self.pid;
        server_chan.peer_pid = pid;
        self.accept_tx
            .try_send(ConnRequest {
                channel: server_chan,
                addr: local_addr,
                pid,
            })
            .map_err(|_| KError::ConnectionRefused)?;
        self.listener_endpoint.polls.readable.wake();
        Ok(client_chan)
    }
}

struct ConnRequest {
    channel: Channel,
    addr: UnixAddr,
    pid: u32,
}

pub struct StreamTransport {
    channel: Mutex<Option<Channel>>,
    accept_rx: Mutex<Option<async_channel::Receiver<ConnRequest>>>,
    /// Handle to the BindEntry's stream slot. Set when this transport is a
    /// listener (bind() was called). On drop, the slot is cleared so the
    /// Bind — and its pending ConnRequests — are released promptly.
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
            accept_rx: Mutex::new(None),
            bind_slot: Mutex::new(None),
            endpoint,
            options: GeneralOptions::default(),
            pid,
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let endpoint1 = Arc::new(StreamEndpoint::default());
        let endpoint2 = Arc::new(StreamEndpoint::default());
        let (chan1, chan2) = new_duplex_channel(endpoint1, endpoint2, pid);
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
                **size = STREAM_BUF_BYTES;
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
        // Clone the Arc handle so we can store it for cleanup on drop.
        let bind_slot_handle = entry.stream.clone();
        let mut slot = entry.stream.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.accept_rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        *slot = Some(Bind {
            accept_tx: tx,
            listener_endpoint: self.endpoint.clone(),
            pid: self.pid,
        });
        drop(slot);
        *guard = Some(rx);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.endpoint.polls.wake_state_change();
        Ok(())
    }

    fn listen(&self, _backlog: usize) -> KResult<()> {
        if self.channel.lock().is_some() || self.accept_rx.lock().is_none() {
            return Err(KError::InvalidInput);
        }
        if !self.endpoint.is_listening.swap(true, Ordering::AcqRel) {
            self.endpoint.polls.wake_state_change();
        }
        Ok(())
    }

    fn connect(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult<()> {
        let mut guard = self.channel.lock();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        *guard = Some(
            slot.stream
                .lock()
                .as_ref()
                .ok_or(KError::NotConnected)?
                .connect(self.endpoint.clone(), local_addr.clone(), self.pid)?,
        );
        self.endpoint.polls.wake_state_change();
        Ok(())
    }

    async fn accept(&self, nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        if !self.endpoint.is_listening.load(Ordering::Acquire) {
            return Err(KError::InvalidInput);
        }
        let rx = {
            let guard = self.accept_rx.lock();
            let Some(rx) = guard.as_ref() else {
                return Err(KError::NotConnected);
            };
            rx.clone()
        };
        let request = if nonblocking {
            rx.try_recv().map_err(|err| match err {
                async_channel::TryRecvError::Empty => KError::WouldBlock,
                async_channel::TryRecvError::Closed => KError::ConnectionReset,
            })?
        } else {
            rx.recv().await.map_err(|_| KError::ConnectionReset)?
        };
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

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
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
                {
                    let _tx_order = self.endpoint.tx_order.lock();
                    if self.endpoint.tx_closed.load(Ordering::Acquire)
                        || chan.peer_endpoint.rx_closed.load(Ordering::Acquire)
                        || !chan.tx.read_is_held()
                    {
                        return finish_send_on_error(total, KError::BrokenPipe);
                    }
                    // SAFETY: `count` is the sum of bytes written into the vacant
                    // slices above, so it never exceeds the producer capacity that
                    // was exposed while the channel lock excluded other producers.
                    unsafe { chan.tx.advance_write_index(count) };
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

    fn recv(&self, mut dst: impl Write, options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let mut guard = self.channel.lock();
                let Some(chan) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };

                let occupied_before = chan.rx.occupied_len();
                let count = {
                    let (left, right) = chan.rx.as_slices();
                    let mut count = dst.write(left)?;
                    if count >= left.len() {
                        count += dst.write(right)?;
                    }
                    // SAFETY: `count` is the sum of bytes copied out of the
                    // occupied slices returned by `as_slices`, so advancing by this
                    // amount stays within the consumer's readable region.
                    unsafe { chan.rx.advance_read_index(count) };
                    count
                };
                if count > 0 {
                    let occupied_after = occupied_before - count;
                    if !is_stream_writable(occupied_before) && is_stream_writable(occupied_after) {
                        chan.peer_endpoint.polls.writable.wake();
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
        let bind_slot = how
            .has_read()
            .then(|| self.bind_slot.lock().clone())
            .flatten();
        let bind_guard = bind_slot.as_ref().map(|slot| slot.lock());
        let is_rx_changed = if how.has_read() {
            let _tx_order = channel
                .as_ref()
                .map(|channel| channel.peer_endpoint.tx_order.lock());
            !self.endpoint.rx_closed.swap(true, Ordering::AcqRel)
        } else {
            false
        };
        drop(bind_guard);
        let is_tx_changed = if how.has_write() {
            let _tx_order = self.endpoint.tx_order.lock();
            !self.endpoint.tx_closed.swap(true, Ordering::AcqRel)
        } else {
            false
        };
        if !is_rx_changed && !is_tx_changed {
            return Ok(());
        }

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
                    !chan.tx.read_is_held() || is_stream_writable(chan.tx.occupied_len()),
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
        } else if let Some(accept_rx) = self.accept_rx.lock().as_ref() {
            if self.endpoint.is_listening.load(Ordering::Acquire) {
                let is_rx_closed = self.endpoint.rx_closed.load(Ordering::Acquire);
                let is_tx_closed = self.endpoint.tx_closed.load(Ordering::Acquire);
                events.set(IoEvents::IN, is_rx_closed || !accept_rx.is_empty());
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

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.endpoint.polls.register(context.waker(), events);
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        // If this transport was a listener, release the Bind from the
        // BindEntry so pending ConnRequests (and their ring buffers) are
        // freed immediately instead of lingering until the inode is unlinked.
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

#[cfg(unittest)]
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
    use kio::Write;
    use kpoll::{IoEvents, PollSet, Pollable};
    use ksync::Mutex;
    use ringbuf::traits::Observer;
    use unittest::{assert, assert_eq, def_test};

    use super::{
        STREAM_BUF_BYTES, STREAM_WRITABLE_MAX_OCCUPIED_BYTES, StreamTransport, UnixTransportOps,
    };
    use crate::{
        RecvOptions, SendFlags, SendOptions, Shutdown,
        options::{Configurable, GetSocketOption},
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

    fn register(socket: &StreamTransport, events: IoEvents) -> Arc<WakeCounter> {
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let mut context = Context::from_waker(&waker);
        socket.register(&mut context, events);
        counter
    }

    fn register_channel_lock_probe(
        poll_set: &PollSet,
        socket: &Arc<StreamTransport>,
    ) -> Arc<ChannelLockProbe> {
        let probe = Arc::new(ChannelLockProbe {
            socket: Arc::downgrade(socket),
            was_unlocked: AtomicBool::new(false),
        });
        poll_set.register(&Waker::from(probe.clone()));
        probe
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
        let local_wake_saw_unlocked =
            register_channel_lock_probe(&left.endpoint.polls.writable, &left);
        let peer_wake_saw_unlocked =
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
        client.connect(&entry, &UnixAddr::Unbound).unwrap();
        assert_eq!(client_read.0.load(Ordering::SeqCst), 1);
    }

    #[def_test]
    fn unix_stream_listener_does_not_leave_a_stale_read_registration() {
        let listener = StreamTransport::new(1);
        let client = StreamTransport::new(2);
        let entry = BindEntry::default();
        listener.bind(&entry, &UnixAddr::Unbound).unwrap();
        listener.listen(1).unwrap();
        let listener_read = register(&listener, IoEvents::IN);

        client.connect(&entry, &UnixAddr::Unbound).unwrap();
        assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);

        listener.endpoint.polls.readable.wake();
        assert_eq!(listener_read.0.load(Ordering::SeqCst), 1);
    }
}
