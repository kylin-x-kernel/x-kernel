//! Vsock socket support.
// pub(crate) mod dgram; todo

pub(crate) mod connection_manager;
pub(crate) mod stream;

use alloc::boxed::Box;
use core::task::Context;

use enum_dispatch::enum_dispatch;
pub use kdriver::prelude::{VsockAddr, VsockConnId};
use kerrno::{KError, KResult};
use kio::{IoBuf, IoBufMut, Read, Write};
use kpoll::{IoEvents, Pollable};

pub use self::stream::VsockStreamTransport;
use crate::{
    RecvOptions, SendOptions, Shutdown, Socket, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

/// Abstract transport trait for vsock sockets.
#[enum_dispatch]
pub trait VsockTransportOps: Configurable + Pollable + Send + Sync {
    fn bind(&self, local_addr: VsockAddr) -> KResult;
    fn listen(&self) -> KResult;
    fn connect(&self, peer_addr: VsockAddr) -> KResult;
    fn accept(&self) -> KResult<(VsockTransport, VsockAddr)>;
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    fn recv(&self, dst: impl Write, options: RecvOptions<'_>) -> KResult<usize>;
    fn shutdown(&self, _how: Shutdown) -> KResult;
    fn local_addr(&self) -> KResult<Option<VsockAddr>>;
    fn peer_addr(&self) -> KResult<Option<VsockAddr>>;
}

#[enum_dispatch(Configurable, VsockTransportOps)]
pub enum VsockTransport {
    Stream(VsockStreamTransport),
    // Dgram(VsockDgramVsockTransport),
}

impl Pollable for VsockTransport {
    fn poll(&self) -> IoEvents {
        match self {
            VsockTransport::Stream(stream) => stream.poll(),
            // VsockTransport::Dgram(dgram) => dgram.poll(),
        }
    }

    fn register(&self, context: &mut core::task::Context<'_>, events: IoEvents) {
        match self {
            VsockTransport::Stream(stream) => stream.register(context, events),
            // VsockTransport::Dgram(dgram) => dgram.register(context, events),
        }
    }
}

/// A network socket using the vsock protocol.
/// A network socket using the vsock protocol.
pub struct VsockSocket {
    transport: VsockTransport,
}

impl VsockSocket {
    /// Create a new vsock socket from a transport.
    pub fn new(transport: impl Into<VsockTransport>) -> Self {
        Self {
            transport: transport.into(),
        }
    }
}

impl Configurable for VsockSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<bool> {
        self.transport.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<bool> {
        self.transport.set_option_inner(opt)
    }
}

impl SocketOps for VsockSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let local_addr = local_addr.into_vsock()?;
        self.transport.bind(local_addr)
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        let remote_addr = remote_addr.into_vsock()?;
        self.transport.connect(remote_addr)
    }

    fn listen(&self) -> KResult {
        self.transport.listen()
    }

    fn accept(&self) -> KResult<Socket> {
        self.transport.accept().map(|(transport, _addr)| {
            let socket = VsockSocket::new(transport);
            Socket::Vsock(Box::new(socket))
        })
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        self.transport.send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        self.transport.recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Vsock(
            self.transport.local_addr()?.ok_or(KError::NotFound)?,
        ))
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Ok(SocketAddrEx::Vsock(
            self.transport.peer_addr()?.ok_or(KError::NotFound)?,
        ))
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        self.transport.shutdown(how)
    }
}

impl Pollable for VsockSocket {
    fn poll(&self) -> IoEvents {
        self.transport.poll()
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.transport.register(context, events);
    }
}
