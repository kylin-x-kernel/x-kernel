// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix seqpacket socket transport.
//!
//! A seqpacket socket is connection-oriented like a stream socket but keeps
//! message boundaries like a datagram socket. Both ends of every connection
//! live in this kernel, so the transport moves whole messages through the same
//! `async_channel` queues the datagram transport uses and adds the connection
//! setup a listener needs: `bind` creates the rendezvous, `listen` enables it,
//! `connect` queues a connection, and `accept` takes one out.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use async_channel::TryRecvError;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use super::dgram::Datagram;
use crate::{
    ConnectOptions, QueueLengths, RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

/// One direction's receive side: the message queue, its poll set, and how many
/// bytes it currently holds.
///
/// Dropping it releases the direction: the queue goes away and the byte count
/// the peer reads as `SIOCOUTQ` falls to zero with it, exactly as Linux's
/// `unix_release_sock` purges the queued skbs and releases the sender's
/// `sk_wmem_alloc`. A `shutdown(SHUT_RD)` only closes the receiver -- the
/// records queued before it stay readable -- so this value, and the count it
/// carries, stay alive until the direction is really released.
struct RxChannel {
    rx: async_channel::Receiver<Datagram>,
    poll: Arc<PollSet>,
    /// Bytes queued in `rx` and not read yet; the peer's `SIOCOUTQ`.
    queued: Arc<AtomicUsize>,
}

impl Drop for RxChannel {
    fn drop(&mut self) {
        // Close before draining: once the close has returned no further send
        // can be accepted, so everything still in the queue is drained and a
        // racing `send` either enqueued before the close (and is drained here)
        // or fails and gives back its own charge. Draining first would let a
        // send slip a record in behind the last empty `try_recv`.
        self.rx.close();
        // Purge what is still queued and release the sender's charge for
        // exactly those bytes, like the skb destructors Linux runs when
        // `unix_release_sock` purges a receive queue. Releasing is a
        // subtraction per record rather than a single store so it composes
        // with a concurrent `send` that is charging its own record: whichever
        // order the two run in, the count ends at the bytes still queued --
        // none.
        while let Ok(datagram) = self.rx.try_recv() {
            self.queued
                .fetch_sub(datagram.data.len(), Ordering::Relaxed);
        }
    }
}

/// Connections a listener may queue before `connect` starts failing.
///
/// Linux applies the `listen` backlog here. The queue is bounded like a listen
/// queue rather than unbounded so an unaccepted connection cannot pin memory.
const LISTEN_BACKLOG: usize = 128;

/// One connection waiting in a listener's queue.
///
/// The client's end of both directions is already wired when the entry is
/// queued, so `connect` returns without waiting for `accept`, exactly like
/// Linux queues the connection and returns.
struct Pending {
    /// What the server reads. Dropping it without an `accept` releases the
    /// client's queued bytes, like the listener release that purges them.
    to_server: RxChannel,
    /// What the server writes.
    from_server: async_channel::Sender<Datagram>,
    /// Bytes queued in `from_server` and not yet read.
    from_server_bytes: Arc<AtomicUsize>,
    /// Poll set the client registers for readability of `from_server`.
    client_poll: Arc<PollSet>,
    client_addr: UnixAddr,
    client_credentials: PeerCredentials,
}

/// Whether a bound socket accepts connections.
///
/// `Unlistened` is distinct from `AcceptShutdown`: the first must fail `accept`
/// immediately, while the second still lets `accept` drain connections queued
/// before `shutdown(SHUT_RD)`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListenerState {
    Unlistened,
    Listening,
    AcceptShutdown,
}

impl ListenerState {
    fn is_listening(self) -> bool {
        self == Self::Listening
    }

    fn is_accept_shutdown(self) -> bool {
        self == Self::AcceptShutdown
    }
}

/// Accept side of a bound socket, shared with the transport while it is bound.
#[derive(Clone)]
struct Listener {
    pending: async_channel::Receiver<Pending>,
    poll: Arc<PollSet>,
    state: Arc<Mutex<ListenerState>>,
}

/// Rendezvous a bound seqpacket socket publishes to `connect`.
pub(crate) struct Bind {
    queue: async_channel::Sender<Pending>,
    poll: Arc<PollSet>,
    /// Accept queue lifecycle, shared with the transport's `Listener`.
    ///
    /// Connecting to a bound-but-not-listening socket fails with
    /// `ECONNREFUSED`, so the address alone cannot decide acceptance.
    state: Arc<Mutex<ListenerState>>,
    /// Identity `listen` recorded; a connecting client reports it as its peer.
    listener_credentials: Mutex<PeerCredentials>,
}

impl Bind {
    fn enqueue(&self, pending: Pending) -> KResult<()> {
        if !self.state.lock().is_listening() {
            return Err(KError::ConnectionRefused);
        }
        self.queue
            .try_send(pending)
            .map_err(|_| KError::ConnectionRefused)?;
        self.poll.wake();
        Ok(())
    }
}

/// Which directions `shutdown` has closed.
#[derive(Clone, Copy, Default)]
struct ShutdownState {
    read: bool,
    write: bool,
}

/// The outgoing direction of a connected socket and the peer's identity.
///
/// `tx` sends into the peer's receive queue and is cleared by
/// `shutdown(SHUT_WR)` rather than dropping the whole record: Linux keeps a
/// connected socket connected after a half-close, so a second `connect` must
/// still fail with `EISCONN` and `SO_PEERCRED` must survive. Clearing the
/// sender still closes the peer's receive channel once the last sender is gone.
struct PeerChannel {
    tx: Option<async_channel::Sender<Datagram>>,
    /// Bytes queued in the channel `tx` feeds and the peer has not read yet.
    ///
    /// This is the sender's `SIOCOUTQ` (`unix_outq_len`: `sk_wmem_alloc`), so
    /// it belongs to the direction, not to either socket.
    tx_bytes: Arc<AtomicUsize>,
    /// Poll set the peer registers for readability of its receive channel.
    read_poll: Arc<PollSet>,
    peer_credentials: PeerCredentials,
}

pub struct SeqpacketTransport {
    /// Incoming messages from the peer.
    rx: Mutex<Option<RxChannel>>,
    /// Outgoing direction and the peer's identity.
    peer: RwLock<Option<PeerChannel>>,
    /// Accepted-connection queue of a listening socket.
    listener: Mutex<Option<Listener>>,
    /// Directions closed by `shutdown`.
    shutdown: Mutex<ShutdownState>,
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
    local_addr: RwLock<UnixAddr>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
}

impl Default for SeqpacketTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SeqpacketTransport {
    /// Accepts a connection using an inline, lazily polled future.
    pub async fn accept(&self, nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        let listener = self.listener.lock().clone().ok_or(KError::InvalidInput)?;
        if *listener.state.lock() == ListenerState::Unlistened {
            return Err(KError::InvalidInput);
        }

        let pending = if nonblocking || self.options.nonblocking() {
            match listener.pending.try_recv() {
                Ok(pending) => pending,
                Err(TryRecvError::Empty) => {
                    let state = *listener.state.lock();
                    return if state.is_listening() {
                        Err(KError::WouldBlock)
                    } else {
                        Err(KError::InvalidInput)
                    };
                }
                Err(TryRecvError::Closed) => return Err(KError::InvalidInput),
            }
        } else {
            listener
                .pending
                .recv()
                .await
                .map_err(|_| KError::InvalidInput)?
        };

        let accepted = Self::new();
        if self.options.pass_credentials() {
            accepted.set_option(SetSocketOption::PassCredentials(&true))?;
        }
        *accepted.rx.lock() = Some(pending.to_server);
        *accepted.peer.write() = Some(PeerChannel {
            tx: Some(pending.from_server),
            tx_bytes: pending.from_server_bytes,
            read_poll: pending.client_poll,
            peer_credentials: pending.client_credentials,
        });
        accepted
            .local_addr
            .write()
            .clone_from(&self.local_addr.read().clone());
        Ok((UnixTransport::Seqpacket(accepted), pending.client_addr))
    }

    pub fn new() -> Self {
        Self {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            listener: Mutex::new(None),
            shutdown: Mutex::new(ShutdownState::default()),
            bind_slot: Mutex::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    /// Creates a connected seqpacket pair, matching `socketpair`.
    pub fn new_pair(credentials: PeerCredentials) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        // `bytes12` counts what `tx2` queued and `rx2` has not read, `bytes21`
        // the other direction; each socket holds its read side's counter in its
        // `rx` slot and the write side's in its peer record.
        let bytes12 = Arc::new(AtomicUsize::new(0));
        let bytes21 = Arc::new(AtomicUsize::new(0));
        let transport1 = Self::new();
        let transport2 = Self::new();
        *transport1.rx.lock() = Some(RxChannel {
            rx: rx1,
            poll: poll1.clone(),
            queued: bytes21.clone(),
        });
        *transport1.peer.write() = Some(PeerChannel {
            tx: Some(tx2),
            tx_bytes: bytes12.clone(),
            read_poll: poll2.clone(),
            peer_credentials: credentials.clone(),
        });
        *transport2.rx.lock() = Some(RxChannel {
            rx: rx2,
            poll: poll2.clone(),
            queued: bytes12.clone(),
        });
        *transport2.peer.write() = Some(PeerChannel {
            tx: Some(tx1),
            tx_bytes: bytes21,
            read_poll: poll1,
            peer_credentials: credentials,
        });
        (transport1, transport2)
    }

    /// Stops a listening socket from queueing new connections.
    ///
    /// Marking the state `AcceptShutdown` stops new connections. Closing the
    /// queue's sender wakes a blocked `accept` once the pending connections
    /// already queued have been drained; `accept` then observes
    /// `TryRecvError::Closed` and returns `EINVAL`, matching Linux after
    /// `shutdown(SHUT_RD)` on a listener.
    fn close_listener_accepts(&self) {
        let Some(handle) = self.bind_slot.lock().clone() else {
            return;
        };
        let poll = {
            let mut slot = handle.lock();
            let Some(bind) = slot.as_mut() else {
                return;
            };
            let mut state = bind.state.lock();
            if !state.is_listening() {
                return;
            }
            *state = ListenerState::AcceptShutdown;
            drop(state);
            bind.queue.close();
            bind.poll.clone()
        };
        poll.wake();
    }
}

impl Configurable for SeqpacketTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

        // `AcceptConn` must be answered before the shared general options: that
        // handler answers it with a constant `false` and reports the option as
        // handled, so delegating first shadowed this transport's own state and a
        // listening seqpacket socket reported 0.
        if let O::AcceptConn(listening) = opt {
            // `unix_listen` accepts SOCK_SEQPACKET like SOCK_STREAM, so
            // `sk_getsockopt` reports `sk_state == TCP_LISTEN` here too.
            **listening = self
                .listener
                .lock()
                .as_ref()
                .map(|listener| listener.state.lock().is_listening())
                .unwrap_or(false);
            return Ok(OptionHandled::Yes);
        }
        if self.options.get_option_inner(opt)?.is_yes() {
            return Ok(OptionHandled::Yes);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                **cred = self
                    .peer
                    .read()
                    .as_ref()
                    .map(|peer| peer.peer_credentials.clone())
                    .unwrap_or_default();
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

impl UnixTransportOps for SeqpacketTransport {
    fn is_listening(&self) -> bool {
        self.listener
            .lock()
            .as_ref()
            .is_some_and(|listener| listener.state.lock().is_listening())
    }

    fn queue_lengths(&self) -> Option<QueueLengths> {
        // `unix_inq_len` sums every queued skb for SOCK_SEQPACKET (unlike
        // datagram, which reports only the head), and `unix_outq_len` reports
        // the sender's queued bytes. Both are plain byte totals, so each
        // direction carries a counter that its writer adds to and its reader
        // subtracts from; no peek is needed. A listening socket is refused for
        // `SIOCINQ` by the ioctl itself (`unix_inq_len`), not here: `SIOCOUTQ`
        // still answers on it.
        let inq = self
            .rx
            .lock()
            .as_ref()
            .map_or(0, |rx| rx.queued.load(Ordering::Relaxed));
        let outq = self
            .peer
            .read()
            .as_ref()
            .map_or(0, |peer| peer.tx_bytes.load(Ordering::Relaxed));
        Some(QueueLengths::from_bytes(outq, inq))
    }

    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let bind_slot_handle = slot.seqpacket.clone();
        let mut bind = bind_slot_handle.lock();
        if bind.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.listener.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::bounded(LISTEN_BACKLOG);
        let poll = Arc::new(PollSet::new());
        let state = Arc::new(Mutex::new(ListenerState::Unlistened));
        *bind = Some(Bind {
            queue: tx,
            poll: poll.clone(),
            state: state.clone(),
            listener_credentials: Mutex::new(PeerCredentials::none()),
        });
        drop(bind);
        *guard = Some(Listener {
            pending: rx,
            poll,
            state,
        });
        self.local_addr.write().clone_from(local_addr);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.poll_state.wake();
        Ok(())
    }

    fn listen(&self, _backlog: usize, credentials: &PeerCredentials) -> KResult {
        let handle = self.bind_slot.lock().clone().ok_or(KError::InvalidInput)?;
        let mut bind = handle.lock();
        let bind = bind.as_mut().ok_or(KError::InvalidInput)?;
        {
            let mut state = bind.state.lock();
            if state.is_listening() || state.is_accept_shutdown() {
                return Err(KError::InvalidInput);
            }
            *state = ListenerState::Listening;
        }
        *bind.listener_credentials.lock() = credentials.clone();
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindEntry,
        _local_addr: &UnixAddr,
        _options: ConnectOptions,
        credentials: &PeerCredentials,
    ) -> KResult {
        // The connected-or-not test and the installation of the connection are
        // one critical section: a second `connect` must observe either the
        // complete connection or nothing, never a half-installed one.
        let mut peer = self.peer.write();
        if peer.is_some() {
            return Err(KError::AlreadyConnected);
        }
        let client_addr = self.local_addr.read().clone();
        let client_credentials = credentials.clone();
        let (to_server, server_rx) = async_channel::unbounded();
        let (from_server, client_rx) = async_channel::unbounded();
        let server_poll = Arc::new(PollSet::new());
        let client_poll = Arc::new(PollSet::new());
        // `to_server_bytes` counts what the client queued and the server has
        // not read: it is the client's `SIOCOUTQ` and the server's `SIOCINQ`.
        // `from_server_bytes` counts the other direction. Each counter is held
        // by both ends -- the sender's peer record charges it, the receiving
        // `RxChannel` releases it -- so the two views cannot disagree.
        let to_server_bytes = Arc::new(AtomicUsize::new(0));
        let from_server_bytes = Arc::new(AtomicUsize::new(0));

        let pending = Pending {
            to_server: RxChannel {
                rx: server_rx,
                // The client's peer record wakes this same poll when the server
                // reads, so the two share it.
                poll: server_poll.clone(),
                queued: to_server_bytes.clone(),
            },
            from_server,
            from_server_bytes: from_server_bytes.clone(),
            client_poll: client_poll.clone(),
            client_addr,
            client_credentials,
        };
        // The bind entry's lock is held across the listening test and the
        // enqueue, so a concurrent `listen` cannot change the verdict between
        // them.
        let bind_slot = slot.seqpacket.lock();
        let bind = bind_slot.as_ref().ok_or(KError::NotConnected)?;
        let listener_credentials = bind.listener_credentials.lock().clone();
        bind.enqueue(pending)?;
        drop(bind_slot);

        // A shutdown may have run before this connection existed. Hold the
        // shutdown lock until both directions are installed so a concurrent
        // shutdown sees either no connection yet or the finished one, never a
        // half-installed connection that misses its state.
        let shutdown = self.shutdown.lock();
        if shutdown.read {
            client_rx.close();
        }
        *self.rx.lock() = Some(RxChannel {
            rx: client_rx,
            poll: client_poll,
            queued: from_server_bytes,
        });
        *peer = Some(PeerChannel {
            tx: (!shutdown.write).then_some(to_server),
            tx_bytes: to_server_bytes,
            read_poll: server_poll,
            // Linux connects seqpacket sockets through the stream path, so the
            // client reports the listening socket's identity as its peer.
            peer_credentials: listener_credentials,
        });
        drop(shutdown);
        self.poll_state.wake();
        Ok(())
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        if options.to.is_some() {
            return Err(KError::InvalidInput);
        }
        let mut data = Vec::new();
        src.read_to_end(&mut data)?;
        let len = data.len();
        let packet = Datagram {
            data,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            sender_credentials: options.sender_credentials,
        };

        if self.shutdown.lock().write {
            return Err(KError::BrokenPipe);
        }
        let connected = self.peer.read();
        let Some(peer) = connected.as_ref() else {
            return Err(KError::NotConnected);
        };
        let Some(tx) = peer.tx.as_ref() else {
            return Err(KError::BrokenPipe);
        };
        // Charge the message before it can be observed in the queue, the way
        // Linux sets `skb_set_owner_w` before `__skb_queue_tail`. Charging
        // after the enqueue would race `RxChannel::drop`'s purge: the purge
        // would release the bytes it drained and the late increment would put
        // them back as a count nobody can read.
        peer.tx_bytes.fetch_add(len, Ordering::Relaxed);
        if tx.try_send(packet).is_err() {
            peer.tx_bytes.fetch_sub(len, Ordering::Relaxed);
            return Err(KError::BrokenPipe);
        }
        peer.read_poll.wake();
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), move || {
                // `shutdown(SHUT_RD)` closes the receiver, but `async_channel`
                // still yields records queued before the close. Draining them
                // first matches Linux, where a read shutdown does not discard
                // data already received. `state.read` also covers a shutdown
                // issued before `connect` installed the receiver.
                let is_read_shutdown = self.shutdown.lock().read;
                let mut guard = self.rx.lock();
                let Some(rx) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };

                let Datagram {
                    data,
                    ancillary,
                    sender,
                    sender_credentials,
                } = match rx.rx.try_recv() {
                    Ok(packet) => packet,
                    Err(TryRecvError::Empty) if is_read_shutdown => return Ok(0),
                    Err(TryRecvError::Empty) => return Err(KError::WouldBlock),
                    Err(TryRecvError::Closed) => return Ok(0),
                };
                rx.queued.fetch_sub(data.len(), Ordering::Relaxed);
                let count = dst.write(&data)?;

                if let Some(from) = options.from.as_mut() {
                    **from = SocketAddrEx::Unix(sender);
                }
                match options.ancillary.as_mut() {
                    Some(dst) => {
                        if self.options.pass_credentials()
                            && let Some(credentials) = sender_credentials
                        {
                            dst.push(Arc::new(crate::KernelAncillaryData::Credentials(
                                credentials,
                            )));
                        }
                        dst.extend(ancillary);
                    }
                    None => {
                        let exposes_credentials =
                            self.options.pass_credentials() && sender_credentials.is_some();
                        if (!ancillary.is_empty() || exposes_credentials)
                            && let Some(discarded) = options.ancillary_discarded.as_deref_mut()
                        {
                            *discarded = true;
                        }
                    }
                }

                // A seqpacket receiver always consumes the whole message; a
                // short buffer truncates it and the remainder is discarded.
                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    data.len()
                } else {
                    count
                })
            })
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        // A seqpacket connection is a pair of message channels. Closing the
        // write direction drops this side's sender, so the peer observes EOF;
        // the connected record stays so `connect` still reports `EISCONN` and
        // `SO_PEERCRED` survives the half-close. Closing the read direction
        // closes this side's receiver, which makes the peer's sends fail while
        // leaving records already queued here available for `recv`.
        let mut state = self.shutdown.lock();
        state.read |= how.has_read();
        state.write |= how.has_write();
        drop(state);

        if how.has_write()
            && let Some(peer) = self.peer.write().as_mut()
        {
            peer.tx.take();
            peer.read_poll.wake();
        }

        if how.has_read() {
            let rx_poll = self.rx.lock().as_ref().map(|rx| {
                rx.rx.close();
                rx.poll.clone()
            });
            if let Some(poll) = rx_poll {
                poll.wake();
            }
            self.close_listener_accepts();
        }
        Ok(())
    }
}

impl Pollable for SeqpacketTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        let state = *self.shutdown.lock();

        // Read the peer before taking `rx`: `connect` holds the peer write
        // lock and then installs `rx`, so nesting the locks in the opposite
        // order here would deadlock.
        let is_peer_rx_closed = self
            .peer
            .read()
            .as_ref()
            .is_none_or(|peer| peer.tx.as_ref().is_none_or(|tx| tx.is_closed()));

        if let Some(rx) = self.rx.lock().as_ref() {
            let is_read_shutdown = state.read || rx.rx.is_closed();
            let is_write_shutdown = state.write || is_peer_rx_closed;
            events.set(IoEvents::IN, !rx.rx.is_empty() || is_read_shutdown);
            events.set(IoEvents::RDHUP, is_read_shutdown);
            // A half-close only ends one direction. `HUP` is reserved for a
            // socket whose read and write directions are both gone, matching
            // `stream` and Linux rather than reporting the peer's `SHUT_WR`
            // as a fully dead connection.
            events.set(IoEvents::HUP, is_read_shutdown && is_write_shutdown);
        } else if let Some(listener) = self.listener.lock().as_ref() {
            let listener_state = *listener.state.lock();
            events.set(IoEvents::IN, !listener.pending.is_empty() || state.read);
            events.set(IoEvents::RDHUP, state.read);
            events.set(IoEvents::HUP, state.read && state.write);
            // Linux does not report POLLOUT while the socket is in TCP_LISTEN,
            // including after a read shutdown that stops new accepts. A bound
            // socket that never called `listen` is not listening and stays
            // writable.
            if listener_state != ListenerState::Unlistened {
                events.set(IoEvents::OUT, false);
            }
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.intersects(IoEvents::IN | IoEvents::RDHUP | IoEvents::HUP) {
            return Ok(());
        }
        if let Some(rx) = self.rx.lock().as_ref() {
            context.register(&rx.poll)?;
        } else if let Some(listener) = self.listener.lock().as_ref() {
            context.register(&listener.poll)?;
        }
        Ok(())
    }
}

impl Drop for SeqpacketTransport {
    fn drop(&mut self) {
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        // `RxChannel::drop` releases the receive side and the byte count that
        // the peer reports as `SIOCOUTQ`.
        self.rx.lock().take();
        if let Some(peer) = self.peer.write().take() {
            peer.read_poll.wake();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/seqpacket/tests.rs");
