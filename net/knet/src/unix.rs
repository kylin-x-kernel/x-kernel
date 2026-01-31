// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix domain socket implementations.
pub(crate) mod dgram;
pub(crate) mod stream;

use alloc::{boxed::Box, sync::Arc};
use core::task::Context;

use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use fs_ng_vfs::NodeType;
use hashbrown::HashMap;
use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, OpenOptions};
use kio::{IoBuf, Read, Write};
use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use ktask::future::{block_on, interruptible};
use lazy_static::lazy_static;

pub use self::{dgram::DgramTransport, stream::StreamTransport};
use crate::{
    RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

#[derive(Default, Clone, Debug)]
pub enum UnixAddr {
    #[default]
    Unbound,
    Abstract(Arc<[u8]>),
    Path(Arc<str>),
}

/// Transport interface for Unix-domain sockets.
#[async_trait]
#[enum_dispatch]
pub trait UnixTransportOps: Configurable + Pollable + Send + Sync {
    fn bind(&self, slot: &BindEntry, local_endpoint: &UnixAddr) -> KResult;
    fn connect(&self, slot: &BindEntry, local_endpoint: &UnixAddr) -> KResult;

    async fn accept(&self) -> KResult<(UnixTransport, UnixAddr)>;

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> KResult<usize>;

    fn shutdown(&self, _how: Shutdown) -> KResult {
        Ok(())
    }
}

#[allow(clippy::large_enum_variant)]
#[enum_dispatch(Configurable, UnixTransportOps)]
pub enum UnixTransport {
    Stream(StreamTransport),
    Dgram(DgramTransport),
}
impl Pollable for UnixTransport {
    fn poll(&self) -> IoEvents {
        match self {
            UnixTransport::Stream(stream) => stream.poll(),
            UnixTransport::Dgram(dgram) => dgram.poll(),
        }
    }

    fn register(&self, context: &mut core::task::Context<'_>, events: IoEvents) {
        match self {
            UnixTransport::Stream(stream) => stream.register(context, events),
            UnixTransport::Dgram(dgram) => dgram.register(context, events),
        }
    }
}

#[derive(Default)]
pub struct BindEntry {
    stream: Mutex<Option<stream::Bind>>,
    dgram: Mutex<Option<dgram::Bind>>,
}

lazy_static! {
    static ref ABSTRACT_BINDINGS: Mutex<HashMap<Arc<[u8]>, BindEntry>> = Mutex::new(HashMap::new());
}

pub(crate) fn lookup_bind_entry<R>(
    addr: &UnixAddr,
    f: impl FnOnce(&BindEntry) -> KResult<R>,
) -> KResult<R> {
    match addr {
        UnixAddr::Unbound => Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => {
            let bindings = ABSTRACT_BINDINGS.lock();
            if let Some(entry) = bindings.get(name) {
                f(entry)
            } else {
                Err(KError::NotFound)
            }
        }
        UnixAddr::Path(path) => {
            let loc = FS_CONTEXT.lock().resolve(path.as_ref())?;
            if loc.metadata()?.node_type != NodeType::Socket {
                return Err(KError::NotASocket);
            }
            f(loc
                .user_data()
                .get::<BindEntry>()
                .ok_or(KError::ConnectionRefused)?
                .as_ref())
        }
    }
}
fn lookup_or_create_bind_entry<R>(
    addr: &UnixAddr,
    f: impl FnOnce(&BindEntry) -> KResult<R>,
) -> KResult<R> {
    match addr {
        UnixAddr::Unbound => Err(KError::InvalidInput),
        UnixAddr::Abstract(name) => {
            let mut bindings = ABSTRACT_BINDINGS.lock();
            f(bindings.entry(name.clone()).or_default())
        }
        UnixAddr::Path(path) => {
            let loc = OpenOptions::new()
                .write(true)
                .create(true)
                .node_type(NodeType::Socket)
                .open(&FS_CONTEXT.lock(), path.as_ref())?
                .into_location();
            if loc.metadata()?.node_type != NodeType::Socket {
                return Err(KError::NotASocket);
            }
            f(loc
                .user_data()
                .get_or_insert_with(BindEntry::default)
                .as_ref())
        }
    }
}

pub struct UnixDomainSocket {
    transport: UnixTransport,
    local_endpoint: Mutex<UnixAddr>,
    peer_endpoint: Mutex<UnixAddr>,
}
impl UnixDomainSocket {
    pub fn new(transport: impl Into<UnixTransport>) -> Self {
        Self {
            transport: transport.into(),
            local_endpoint: Mutex::new(UnixAddr::Unbound),
            peer_endpoint: Mutex::new(UnixAddr::Unbound),
        }
    }
}
impl Configurable for UnixDomainSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<bool> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<bool> {
        self.transport.set_option_inner(opt)
    }
}
impl SocketOps for UnixDomainSocket {
    fn bind(&self, local_endpoint: SocketAddrEx) -> KResult {
        let local_endpoint = local_endpoint.into_unix()?;
        let mut local_guard = self.local_endpoint.lock();
        if matches!(&*local_guard, UnixAddr::Unbound) {
            lookup_or_create_bind_entry(&local_endpoint, |slot| {
                self.transport.bind(slot, &local_endpoint)
            })?;
            *local_guard = local_endpoint;
        } else {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_unix()?;
        let local_endpoint = self.local_endpoint.lock().clone();
        let mut peer_guard = self.peer_endpoint.lock();
        if matches!(&*peer_guard, UnixAddr::Unbound) {
            lookup_bind_entry(&remote_addr, |slot| {
                self.transport.connect(slot, &local_endpoint)
            })?;
            *peer_guard = remote_addr;
        } else {
            return Err(KError::InvalidInput);
        }
        Ok(())
    }

    fn listen(&self) -> KResult {
        Ok(())
    }

    fn accept(&self) -> KResult<Socket> {
        let (transport, peer_endpoint) = block_on(interruptible(self.transport.accept()))??;
        Ok(Socket::Unix(Box::new(Self {
            transport,
            local_endpoint: Mutex::new(self.local_endpoint.lock().clone()),
            peer_endpoint: Mutex::new(peer_endpoint),
        })))
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> KResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.local_endpoint.lock().clone()))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Unix(self.peer_endpoint.lock().clone()))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.transport.shutdown(how)
    }
}

impl Pollable for UnixDomainSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.transport.register(context, events);
    }
}
