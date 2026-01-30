//! Unix datagram socket transport.
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::task::Context;

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{Read, Write};
use kpoll::{IoEvents, PollSet, Pollable};
use ksync::Mutex;
use spin::RwLock;

use crate::{
    CMsgData, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, SetSocketOption, UnixCredentials},
    unix::{UnixAddr, UnixTransport, UnixTransportOps, lookup_bind_entry},
};

struct Datagram {
    data: Vec<u8>,
    cmsg: Vec<CMsgData>,
    sender: UnixAddr,
}

struct Channel {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}

pub struct Bind {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.tx.clone();
        Channel {
            tx,
            poll: self.poll.clone(),
        }
    }
}

pub struct DgramTransport {
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
    pid: u32,
}
impl DgramTransport {
    pub fn new(pid: u32) -> Self {
        DgramTransport {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    fn new_connected(
        rx: (async_channel::Receiver<Datagram>, Arc<PollSet>),
        peer: Channel,
        pid: u32,
    ) -> Self {
        DgramTransport {
            rx: Mutex::new(Some(rx)),
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
            pid,
        }
    }

    pub fn new_pair(pid: u32) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
            },
            pid,
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
            },
            pid,
        );
        (transport1, transport2)
    }
}

impl Configurable for DgramTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<bool> {
        use GetSocketOption as O;

        if self.options.get_option_inner(opt)? {
            return Ok(true);
        }

        match opt {
            O::PassCredentials(_) => {}
            O::PeerCredentials(cred) => {
                // Datagram sockets are stateless and do not have a peer, so we
                // return the credentials of the process that created the
                // socket.
                **cred = UnixCredentials::new(self.pid);
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
impl UnixTransportOps for DgramTransport {
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let mut slot = slot.dgram.lock();
        if slot.is_some() {
            return Err(KError::AddrInUse);
        }
        let mut guard = self.rx.lock();
        if guard.is_some() {
            return Err(KError::InvalidInput);
        }
        let (tx, rx) = async_channel::unbounded();
        let poll = Arc::new(PollSet::new());
        *slot = Some(Bind {
            tx,
            poll: poll.clone(),
        });
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(&self, slot: &super::BindEntry, _local_addr: &UnixAddr) -> KResult {
        let mut guard = self.peer.write();
        if guard.is_some() {
            return Err(KError::AlreadyConnected);
        }
        *guard = Some(
            slot.dgram
                .lock()
                .as_ref()
                .ok_or(KError::NotConnected)?
                .connect(),
        );
        self.poll_state.wake();
        Ok(())
    }

    async fn accept(&self) -> KResult<(UnixTransport, UnixAddr)> {
        Err(KError::InvalidInput)
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        let packet = Datagram {
            data: message,
            cmsg: options.cmsg,
            sender: self.local_addr.read().clone(),
        };

        let connected = self.peer.read();
        if let Some(addr) = options.to {
            let addr = addr.into_unix()?;
            lookup_bind_entry(&addr, |slot| {
                if let Some(bind) = slot.dgram.lock().as_ref() {
                    bind.tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
                    bind.poll.wake();
                    Ok(())
                } else {
                    Err(KError::NotConnected)
                }
            })?;
        } else if let Some(chan) = connected.as_ref() {
            chan.tx.try_send(packet).map_err(|_| KError::BrokenPipe)?;
            chan.poll.wake();
        } else {
            return Err(KError::NotConnected);
        }
        Ok(len)
    }

    fn recv(&self, mut dst: impl Write, mut options: RecvOptions) -> KResult<usize> {
        self.options.recv_poller(self, move || {
            let mut guard = self.rx.lock();
            let Some((rx, _)) = guard.as_mut() else {
                return Err(KError::NotConnected);
            };

            let Datagram { data, cmsg, sender } = match rx.try_recv() {
                Ok(packet) => packet,
                Err(TryRecvError::Empty) => {
                    return Err(KError::WouldBlock);
                }
                Err(TryRecvError::Closed) => {
                    return Ok(0);
                }
            };
            let count = dst.write(&data)?;
            if count < data.len() {
                warn!("UDP message truncated: {} -> {} bytes", data.len(), count);
            }

            if let Some(from) = options.from.as_mut() {
                **from = SocketAddrEx::Unix(sender);
            }
            if let Some(dst) = options.cmsg.as_mut() {
                dst.extend(cmsg);
            }

            Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                data.len()
            } else {
                count
            })
        })
    }
}

impl Pollable for DgramTransport {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if let Some((rx, _)) = self.rx.lock().as_ref() {
            events.set(IoEvents::IN, !rx.is_empty());
        }
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if let Some((_, poll)) = self.rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            poll.register(context.waker());
        }
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}
