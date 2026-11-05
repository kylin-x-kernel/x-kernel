// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix datagram socket transport.
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use async_channel::TryRecvError;
use async_trait::async_trait;
use kerrno::{KError, KResult};
use kio::{IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::{Mutex, RwLock};

use crate::{
    AncillaryData, ConnectOptions, RecvFlags, RecvOptions, SendOptions, SocketAddrEx,
    general::GeneralOptions,
    options::{Configurable, GetSocketOption, OptionHandled, PeerCredentials, SetSocketOption},
    unix::{UnixAddr, UnixTransport, UnixTransportOps, lookup_bind_entry},
};

pub(crate) struct Datagram {
    pub(crate) data: Vec<u8>,
    pub(crate) ancillary: Vec<AncillaryData>,
    pub(crate) sender: UnixAddr,
    pub(crate) sender_credentials: Option<crate::SenderCredentials>,
}

pub(crate) struct Channel {
    pub(crate) tx: async_channel::Sender<Datagram>,
    pub(crate) poll: Arc<PollSet>,
    /// Established only for socketpair peers; datagram `bind` and `connect`
    /// leave Linux `sk_peer_pid` unset.
    pub(crate) peer_credentials: PeerCredentials,
}

pub(crate) struct Bind {
    tx: async_channel::Sender<Datagram>,
    poll: Arc<PollSet>,
}
impl Bind {
    fn connect(&self) -> Channel {
        let tx = self.tx.clone();
        Channel {
            tx,
            poll: self.poll.clone(),
            peer_credentials: PeerCredentials::none(),
        }
    }
}

pub struct DgramTransport {
    rx: Mutex<Option<(async_channel::Receiver<Datagram>, Arc<PollSet>)>>,
    peer: RwLock<Option<Channel>>,
    local_addr: RwLock<UnixAddr>,
    bind_slot: Mutex<Option<Arc<Mutex<Option<Bind>>>>>,
    poll_state: Arc<PollSet>,
    options: GeneralOptions,
}
impl Default for DgramTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl DgramTransport {
    pub fn new() -> Self {
        DgramTransport {
            rx: Mutex::new(None),
            peer: RwLock::new(None),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    fn new_connected(rx: (async_channel::Receiver<Datagram>, Arc<PollSet>), peer: Channel) -> Self {
        DgramTransport {
            rx: Mutex::new(Some(rx)),
            peer: RwLock::new(Some(peer)),
            local_addr: RwLock::new(UnixAddr::Unbound),
            bind_slot: Mutex::new(None),
            poll_state: Arc::default(),
            options: GeneralOptions::default(),
        }
    }

    /// Creates a connected datagram pair whose endpoints both report the
    /// creator's peer credentials, matching Linux socketpair.
    pub fn new_pair(credentials: PeerCredentials) -> (Self, Self) {
        let (tx1, rx1) = async_channel::unbounded();
        let (tx2, rx2) = async_channel::unbounded();
        let poll1 = Arc::new(PollSet::new());
        let poll2 = Arc::new(PollSet::new());
        let transport1 = DgramTransport::new_connected(
            (rx1, poll1.clone()),
            Channel {
                tx: tx2,
                poll: poll2.clone(),
                peer_credentials: credentials.clone(),
            },
        );
        let transport2 = DgramTransport::new_connected(
            (rx2, poll2.clone()),
            Channel {
                tx: tx1,
                poll: poll1.clone(),
                peer_credentials: credentials,
            },
        );
        (transport1, transport2)
    }
}

impl Configurable for DgramTransport {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;

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
                    .map(|channel| channel.peer_credentials.clone())
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
#[async_trait]
impl UnixTransportOps for DgramTransport {
    fn bind(&self, slot: &super::BindEntry, local_addr: &UnixAddr) -> KResult {
        let bind_slot_handle = slot.dgram.clone();
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
        drop(slot);
        *guard = Some((rx, poll));
        self.local_addr.write().clone_from(local_addr);
        *self.bind_slot.lock() = Some(bind_slot_handle);
        self.poll_state.wake();
        Ok(())
    }

    fn connect(
        &self,
        slot: &super::BindEntry,
        _local_addr: &UnixAddr,
        _options: ConnectOptions,
        _credentials: &PeerCredentials,
    ) -> KResult {
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

    async fn accept(&self, _nonblocking: bool) -> KResult<(UnixTransport, UnixAddr)> {
        Err(KError::InvalidInput)
    }

    fn send(&self, mut src: impl Read, options: SendOptions) -> KResult<usize> {
        let mut message = Vec::new();
        src.read_to_end(&mut message)?;
        let len = message.len();
        let packet = Datagram {
            data: message,
            ancillary: options.ancillary,
            sender: self.local_addr.read().clone(),
            sender_credentials: options.sender_credentials,
        };

        let connected = self.peer.read();
        if let Some(addr) = options.to {
            let addr = addr.into_unix()?;
            let cred = kprocess::current_cred();
            lookup_bind_entry(&addr, &cred, |slot| {
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

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions) -> KResult<usize> {
        self.options
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), move || {
                let mut guard = self.rx.lock();
                let Some((rx, _)) = guard.as_mut() else {
                    return Err(KError::NotConnected);
                };

                let Datagram {
                    data,
                    ancillary,
                    sender,
                    sender_credentials,
                } = match rx.try_recv() {
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
                match options.ancillary.as_mut() {
                    Some(dst) => {
                        // Deliver identity before passed files so a short control
                        // buffer cannot spend the credential space on descriptors.
                        if self.options.pass_credentials()
                            && let Some(credentials) = sender_credentials
                        {
                            dst.push(Arc::new(crate::KernelAncillaryData::Credentials(
                                credentials,
                            )));
                        }
                        dst.extend(ancillary);
                    }
                    // The receive gave no control buffer: the datagram is consumed
                    // either way, but its control data cannot be delivered. Report
                    // the loss so the caller sets `MSG_CTRUNC`, like Linux
                    // `__scm_recv_common`, which also covers `SO_PASSCRED` sender
                    // credentials and not only passed file descriptions.
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

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Some((_, poll)) = self.rx.lock().as_ref()
            && events.contains(IoEvents::IN)
        {
            context.register(poll)?;
        }
        Ok(())
    }
}

impl Drop for DgramTransport {
    fn drop(&mut self) {
        if let Some(slot) = self.bind_slot.lock().take() {
            *slot.lock() = None;
        }
        if let Some(chan) = self.peer.write().take() {
            chan.poll.wake();
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/unix/dgram/tests.rs");
