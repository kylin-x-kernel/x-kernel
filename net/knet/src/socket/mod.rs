// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common socket wrapper types and poll helpers.
pub(crate) mod file;
pub(crate) mod general;
pub mod options;
pub(crate) mod state;

#[cfg(unittest)]
mod test_options;
#[cfg(unittest)]
mod test_state;

use alloc::{boxed::Box, vec::Vec};
use core::{
    any::Any,
    fmt::{self, Debug},
    net::SocketAddr,
};

use bitflags::bitflags;
use enum_dispatch::enum_dispatch;
#[cfg(feature = "vsock")]
use kclass::prelude::VsockAddr;
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;

#[cfg(feature = "vsock")]
use crate::vsock::VsockSocket;
use crate::{
    netlink::{NetlinkAddr, NetlinkSocket},
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    packet::{PacketAddr, PacketSocket},
    raw::RawSocket,
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{UnixAddr, UnixDomainSocket},
};

#[derive(Clone, Debug)]
pub enum SocketAddrEx {
    Ip(SocketAddr),
    Unix(UnixAddr),
    Netlink(NetlinkAddr),
    Packet(PacketAddr),
    #[cfg(feature = "vsock")]
    Vsock(VsockAddr),
}

impl SocketAddrEx {
    pub fn into_ip(self) -> KResult<SocketAddr> {
        match self {
            SocketAddrEx::Ip(addr) => Ok(addr),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_unix(self) -> KResult<UnixAddr> {
        match self {
            SocketAddrEx::Unix(addr) => Ok(addr),
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_netlink(self) -> KResult<NetlinkAddr> {
        match self {
            SocketAddrEx::Netlink(addr) => Ok(addr),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_packet(self) -> KResult<PacketAddr> {
        match self {
            SocketAddrEx::Packet(addr) => Ok(addr),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    #[cfg(feature = "vsock")]
    pub fn into_vsock(self) -> KResult<VsockAddr> {
        match self {
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
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
        /// Do not block for this send operation.
        const DONT_WAIT = 0x01;
    }
}

impl SendFlags {
    pub(crate) fn nonblocking(self) -> bool {
        self.contains(Self::DONT_WAIT)
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
        /// Receive a pending asynchronous error instead of data.
        const ERROR_QUEUE = 0x04;
        /// Do not block for this receive operation.
        const DONT_WAIT = 0x08;
    }
}

impl RecvFlags {
    pub(crate) fn nonblocking(self) -> bool {
        self.contains(Self::DONT_WAIT)
    }
}

pub type AncillaryData = Box<dyn Any + Send + Sync>;

#[derive(Debug, Clone, Copy)]
pub enum SocketErrorOrigin {
    Local,
    Icmp,
    Icmp6,
    TxStatus,
}

/// A protocol error stored in a socket error queue.
#[derive(Debug, Clone)]
pub struct SocketErrorInfo {
    pub errno: LinuxError,
    pub origin: SocketErrorOrigin,
    pub error_type: u8,
    pub error_code: u8,
    pub info: u32,
    pub data: u32,
    pub offender: Option<SocketAddr>,
}

/// Ancillary data produced by the networking stack itself.
#[derive(Debug, Clone)]
pub enum KernelAncillaryData {
    IpError(SocketErrorInfo),
}

/// Options for sending data to a socket.
///
/// See [`SocketOps::send`].
#[derive(Default, Debug)]
pub struct SendOptions {
    pub to: Option<SocketAddrEx>,
    pub flags: SendFlags,
    pub ancillary: Vec<AncillaryData>,
}

/// Options for receiving data from a socket.
///
/// See [`SocketOps::recv`].
#[derive(Default)]
pub struct RecvOptions<'a> {
    pub from: Option<&'a mut SocketAddrEx>,
    pub flags: RecvFlags,
    pub ancillary: Option<&'a mut Vec<AncillaryData>>,
    pub out_flags: Option<&'a mut RecvFlags>,
}
impl Debug for RecvOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvOptions")
            .field("from", &self.from)
            .field("flags", &self.flags)
            .field("out_flags", &self.out_flags.as_ref().map(|_| ()))
            .finish()
    }
}

/// Kind of shutdown operation to perform on a socket.
#[derive(Debug, Clone, Copy, PartialEq)]
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
    fn listen(&self, _backlog: usize) -> KResult {
        Err(KError::OperationNotSupported)
    }
    /// Accepts a connection on a listening socket, returning a new socket.
    fn accept(&self) -> KResult<Socket> {
        Err(KError::OperationNotSupported)
    }

    /// Send data to the socket, optionally to a specific address.
    ///
    /// Stream sockets should consume the full input in blocking mode when
    /// progress remains possible, and may return a partial count after a
    /// successful write when the operation would block or fail.
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

    fn listen(&self, backlog: usize) -> KResult {
        (**self).listen(backlog)
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
    Raw(Box<RawSocket>),
    Unix(Box<UnixDomainSocket>),
    Netlink(Box<NetlinkSocket>),
    Packet(Box<PacketSocket>),
    #[cfg(feature = "vsock")]
    Vsock(Box<VsockSocket>),
}
