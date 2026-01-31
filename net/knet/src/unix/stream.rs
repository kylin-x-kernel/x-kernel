// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix stream socket transport.
use alloc::{boxed::Box, sync::Arc};
use core::{
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBuf, Read, Write};
use kpoll::{IoEvents, PollSet, Pollable};
use ksync::Mutex;
use ringbuf::{
    HeapCons, HeapProd, HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};

use crate::{
    RecvOptions, SendOptions, Shutdown,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps},
};

const STREAM_BUF_BYTES: usize = 64 * 1024;

fn new_ring_pair() -> (HeapProd<u8>, HeapCons<u8>) {
    let rb = HeapRb::new(STREAM_BUF_BYTES);
    rb.split()
}
fn new_duplex_channel(pid: u32) -> (Channel, Channel) {
    let (client_tx, server_rx) = new_ring_pair();
    let (server_tx, client_rx) = new_ring_pair();
    let poll = Arc::new(PollSet::new());
    (
        Channel {
            tx: client_tx,
            rx: client_rx,
            poll: poll.clone(),
            peer_pid: pid,
        },
        Channel {
            tx: server_tx,
            rx: server_rx,
            poll,
            peer_pid: pid,
        },
    )
}

struct Channel {
    tx: HeapProd<u8>,
    rx: HeapCons<u8>,
    // TODO: granularity
    poll: Arc<PollSet>,
    peer_pid: u32,
}

pub struct Bind {
    /// New connections are sent to this channel.
    accept_tx: async_channel::Sender<ConnRequest>,
    accept_poll: Arc<PollSet>,
    pid: u32,
}
impl Bind {
    fn connect(&self, local_addr: UnixAddr, pid: u32) -> KResult<Channel> {
        let (mut client_chan, mut server_chan) = new_duplex_channel(0);
        client_chan.peer_pid = self.pid;
        server_chan.peer_pid = pid;
        self.accept_tx
            .try_send(ConnRequest {
                channel: server_chan,
                addr: local_addr,
                pid,
            })
            .map_err(|_| KError::ConnectionRefused)?;
        self.accept_poll.wake();
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
    accept_rx: Mutex<Option<(async_channel::Receiver<ConnRequest>, Arc<PollSet>)>>,
    poll_state: PollSet,
    options: GeneralOptions,
    pid: u32,
    rx_closed: AtomicBool,
    tx_closed: AtomicBool,
}
impl StreamTransport {
    pub fn new(pid: u32) -> Self {
        StreamTransport::new_channel(None, pid)
    }

    fn new_channel(channel: Option<Channel>, pid: u32) -> Self {
        StreamTransport {
            channel: Mutex::new(channel),
            accept_rx: Mutex::new(None),
            poll_state: PollSet::new(),
            options: GeneralOptions::default(),
            pid,
            rx_closed: AtomicBool::new(false),
            tx_closed: AtomicBool::new(false),
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (chan1, chan2) = new_duplex_channel(pid);
        let transport1 = StreamTransport::new_channel(Some(chan1), pid);
        let transport2 = StreamTransport::new_channel(Some(chan2), pid);
        (transport1, transport2)
    }
}

impl Configurable for StreamTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<bool> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)? {
            return Ok(true);
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
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<bool> {
        use SetSocketOption as O;

        if self.options.set_option_inner(opt)? {
            return Ok(true);
        }

        match opt {
            O::PassCredentials(_) => {}
            _ => return Ok(false),
        }
        Ok(true)
    }
}
#[async_trait]
impl UnixTransportOps for StreamTransport {
    fn bind(&self, slot: &super::BindEntry, _local_addr: &UnixAddr) -> KResult<()> {
        let mut slot = slot.stream.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.accept_rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            accept_tx: tx,
            accept_poll: poll.clone(),
            pid: self.pid,
        });
        *guard = Some((rx, poll));
        self.poll_state.wake();
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
                .connect(local_addr.clone(), self.pid)?,
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> KResult<(UnixTransport, UnixAddr)> {
        let (rx, _poll) = {
            let mut guard = self.accept_rx.lock();
            let Some((rx, poll)) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };
            (rx.clone(), poll.clone())
        };
        let ConnRequest {
            channel,
            addr: peer_addr,
            pid,
        } = rx.recv().await.map_err(|_| KError::ConnectionReset)?;
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
        let non_blocking = self.options.nonblocking();
        self.options.send_poller(self, || {
            let mut guard = self.channel.lock();
            let Some(chan) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };
            if !chan.tx.read_is_held() {
                return Err(KError::BrokenPipe);
            }

            let count = {
                let (left, right) = chan.tx.vacant_slices_mut();
                let mut count = src.read(unsafe { left.assume_init_mut() })?;
                if count >= left.len() {
                    count += src.read(unsafe { right.assume_init_mut() })?;
                }
                unsafe { chan.tx.advance_write_index(count) };
                count
            };
            total += count;
            if count > 0 {
                chan.poll.wake();
            }

            if count == size || non_blocking {
                Ok(total)
            } else {
                Err(KError::WouldBlock)
            }
        })
    }

    fn recv(&self, mut dst: impl Write, _options: RecvOptions) -> KResult<usize> {
        self.options.recv_poller(self, || {
            let mut guard = self.channel.lock();
            let Some(chan) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };

            let count = {
                let (left, right) = chan.rx.as_slices();
                let mut count = dst.write(left)?;
                if count >= left.len() {
                    count += dst.write(right)?;
                }
                unsafe { chan.rx.advance_read_index(count) };
                count
            };
            if count > 0 {
                chan.poll.wake();
                return Ok(count);
            }
            if self.rx_closed.load(Ordering::Acquire) {
                return Ok(0);
            }
            Err(KError::WouldBlock)
        })
    }

    fn shutdown(&self, how: Shutdown) -> KResult<()> {
        if how.has_read() {
            self.rx_closed.store(true, Ordering::Release);
            self.poll_state.wake();
        }
        if how.has_write() {
            self.tx_closed.store(true, Ordering::Release);
            self.poll_state.wake();
        }
        if self.rx_closed.load(Ordering::Acquire)
            && self.tx_closed.load(Ordering::Acquire)
            && let Some(chan) = self.channel.lock().take()
        {
            chan.poll.wake();
        }
        Ok(())
    }
}

impl Pollable for StreamTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        if let Some(chan) = self.channel.lock().as_ref() {
            events.set(
                IoEvents::IN,
                !self.rx_closed.load(Ordering::Acquire) && chan.rx.occupied_len() > 0,
            );
            events.set(
                IoEvents::OUT,
                !self.tx_closed.load(Ordering::Acquire) && chan.tx.vacant_len() > 0,
            );
        } else if let Some((accept_rx, _)) = self.accept_rx.lock().as_ref() {
            events.set(IoEvents::IN, !accept_rx.is_empty());
        }
        events.set(IoEvents::RDHUP, self.rx_closed.load(Ordering::Acquire));
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if let Some(chan) = self.channel.lock().as_ref() {
            if events.intersects(IoEvents::IN | IoEvents::OUT) {
                chan.poll.register(context.waker());
            }
        } else if let Some((_, accept_poll)) = self.accept_rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            accept_poll.register(context.waker());
        }
        self.poll_state.register(context.waker());
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.channel.lock().as_ref() {
            chan.poll.wake();
        }
    }
}
