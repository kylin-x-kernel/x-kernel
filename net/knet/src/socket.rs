//! Common socket wrapper types and poll helpers.
use alloc::{boxed::Box, vec::Vec};
use core::{
    any::Any,
    fmt::{self, Debug},
    net::SocketAddr,
    task::Context,
};

use bitflags::bitflags;
use enum_dispatch::enum_dispatch;
#[cfg(feature = "vsock")]
use kdriver::prelude::VsockAddr;
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, Pollable};

#[cfg(feature = "vsock")]
use crate::vsock::VsockSocket;
use crate::{
    options::{Configurable, GetSocketOption, SetSocketOption},
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{UnixAddr, UnixDomainSocket},
};

#[derive(Clone, Debug)]
pub enum SocketAddrEx {
    Ip(SocketAddr),
    Unix(UnixAddr),
    #[cfg(feature = "vsock")]
    Vsock(VsockAddr),
}

impl SocketAddrEx {
    pub fn into_ip(self) -> KResult<SocketAddr> {
        match self {
            SocketAddrEx::Ip(addr) => Ok(addr),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_unix(self) -> KResult<UnixAddr> {
        match self {
            SocketAddrEx::Unix(addr) => Ok(addr),
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    #[cfg(feature = "vsock")]
    pub fn into_vsock(self) -> KResult<VsockAddr> {
        match self {
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Vsock(addr) => Ok(addr),
        }
    }
}

bitflags! {
    /// Flags for sending data to a socket.
    ///
    /// See [`SocketOps::send`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SendFlags: u32 {
    }
}

bitflags! {
    /// Flags for receiving data from a socket.
    ///
    /// See [`SocketOps::recv`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct RecvFlags: u32 {
        /// Receive data without removing it from the queue.
        const PEEK = 0x01;
        /// For datagram-like sockets, requires [`SocketOps::recv`] to return
        /// the real size of the datagram, even when it is larger than the
        /// buffer.
        const TRUNCATE = 0x02;
    }
}

pub type CMsgData = Box<dyn Any + Send + Sync>;

/// Options for sending data to a socket.
///
/// See [`SocketOps::send`].
#[derive(Default, Debug)]
pub struct SendOptions {
    pub to: Option<SocketAddrEx>,
    pub flags: SendFlags,
    pub cmsg: Vec<CMsgData>,
}

/// Options for receiving data from a socket.
///
/// See [`SocketOps::recv`].
#[derive(Default)]
pub struct RecvOptions<'a> {
    pub from: Option<&'a mut SocketAddrEx>,
    pub flags: RecvFlags,
    pub cmsg: Option<&'a mut Vec<CMsgData>>,
}
impl Debug for RecvOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvOptions")
            .field("from", &self.from)
            .field("flags", &self.flags)
            .finish()
    }
}

/// Kind of shutdown operation to perform on a socket.
#[derive(Debug, Clone, Copy)]
pub enum Shutdown {
    Read,
    Write,
    Both,
}
impl Shutdown {
    pub fn has_read(&self) -> bool {
        matches!(self, Shutdown::Read | Shutdown::Both)
    }

    pub fn has_write(&self) -> bool {
        matches!(self, Shutdown::Write | Shutdown::Both)
    }
}

/// Operations that can be performed on a socket.
#[enum_dispatch]
pub trait SocketOps: Configurable {
    /// Binds an unbound socket to the given address and port.
    fn bind(&self, local_addr: SocketAddrEx) -> KResult;
    /// Connects the socket to a remote address.
    fn connect(&self, remote_addr: SocketAddrEx) -> KResult;

    /// Starts listening on the bound address and port.
    fn listen(&self) -> KResult {
        Err(KError::OperationNotSupported)
    }
    /// Accepts a connection on a listening socket, returning a new socket.
    fn accept(&self) -> KResult<Socket> {
        Err(KError::OperationNotSupported)
    }

    /// Send data to the socket, optionally to a specific address.
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    /// Receive data from the socket.
    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize>;

    /// Get the local endpoint of the socket.
    fn local_addr(&self) -> KResult<SocketAddrEx>;
    /// Get the remote endpoint of the socket.
    fn peer_addr(&self) -> KResult<SocketAddrEx>;

    /// Shutdown the socket, closing the connection.
    fn shutdown(&self, how: Shutdown) -> KResult;
}

impl<T: SocketOps + ?Sized> SocketOps for Box<T> {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        (**self).bind(local_addr)
    }

    fn connect(&self, remote_addr: SocketAddrEx) -> KResult {
        (**self).connect(remote_addr)
    }

    fn listen(&self) -> KResult {
        (**self).listen()
    }

    fn accept(&self) -> KResult<Socket> {
        (**self).accept()
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        (**self).send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        (**self).recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        (**self).local_addr()
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        (**self).peer_addr()
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        (**self).shutdown(how)
    }
}

/// Network socket abstraction.
#[allow(clippy::large_enum_variant)]
#[enum_dispatch(Configurable, SocketOps)]
pub enum Socket {
    Udp(Box<UdpSocket>),
    Tcp(Box<TcpSocket>),
    Unix(Box<UnixDomainSocket>),
    #[cfg(feature = "vsock")]
    Vsock(Box<VsockSocket>),
}

impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        match self {
            Socket::Tcp(tcp) => tcp.poll(),
            Socket::Udp(udp) => udp.poll(),
            Socket::Unix(unix) => unix.poll(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.poll(),
        }
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        match self {
            Socket::Tcp(tcp) => tcp.register(context, events),
            Socket::Udp(udp) => udp.register(context, events),
            Socket::Unix(unix) => unix.register(context, events),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.register(context, events),
        }
    }
}
