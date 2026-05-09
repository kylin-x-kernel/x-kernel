// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket creation and management syscalls.
//!
//! This module implements socket operations including:
//! - Socket creation (socket, socketpair, etc.)
//! - Socket configuration (setsockopt, getsockopt, etc.)
//! - Socket binding and connection (bind, connect, listen, etc.)
//! - Socket shutdown (shutdown, etc.)

use alloc::{boxed::Box, sync::Arc};

use kerrno::{KError, KResult, LinuxError};
#[cfg(feature = "vsock")]
use knet::vsock::{VsockSocket, VsockStreamTransport};
use knet::{
    Shutdown, SocketAddrEx, SocketOps,
    netlink::NetlinkSocket,
    raw::{IpProtocol, IpVersion, RawSocket},
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{DgramTransport, StreamTransport, UnixDomainSocket},
};
use kservices::mm::{UserConstPtr, UserPtr};
use linux_raw_sys::{
    general::{O_CLOEXEC, O_NONBLOCK},
    net::{
        AF_INET, AF_INET6, AF_NETLINK, AF_UNIX, AF_VSOCK, IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP,
        SHUT_RD, SHUT_RDWR, SHUT_WR, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET, SOCK_STREAM, sockaddr,
        socklen_t,
    },
};

use crate::{
    file::{FileLike, Socket},
    socket::SocketAddrExt,
};

/// Create a new socket of the specified domain, type, and protocol
pub fn sys_socket(domain: u32, raw_ty: u32, proto: u32) -> KResult<isize> {
    // Extract the type bits (lower 8 bits, ignoring flags like SOCK_CLOEXEC)
    let ty = raw_ty & 0xFF;

    let pid = kthread::current_thread().pid();
    // Create the appropriate socket type based on domain and type
    let socket = match (domain, ty) {
        (AF_INET, SOCK_STREAM) => {
            // TCP socket - verify protocol if specified
            if proto != 0 && proto != IPPROTO_TCP as u32 {
                return Err(KError::from(LinuxError::EPROTONOSUPPORT));
            }
            knet::Socket::Tcp(Box::new(TcpSocket::new()))
        }
        (AF_INET, SOCK_DGRAM) => {
            // UDP socket - verify protocol if specified
            if proto != 0 && proto != IPPROTO_UDP as u32 {
                return Err(KError::from(LinuxError::EPROTONOSUPPORT));
            }
            knet::Socket::Udp(Box::new(UdpSocket::new()))
        }
        (AF_INET, SOCK_RAW) => {
            if proto != IPPROTO_ICMP as u32 {
                return Err(KError::from(LinuxError::EPROTONOSUPPORT));
            }
            knet::Socket::Raw(Box::new(RawSocket::new(IpVersion::Ipv4, IpProtocol::Icmp)))
        }
        // IPv6 is not exposed yet because the address family lacks full support.
        // The previous raw ICMPv6-only path was removed with this policy.
        (AF_INET6, _) => return Err(KError::from(LinuxError::EAFNOSUPPORT)),
        (AF_UNIX, SOCK_STREAM) => {
            // Unix domain stream socket
            knet::Socket::Unix(Box::new(UnixDomainSocket::new(StreamTransport::new(pid))))
        }
        (AF_UNIX, SOCK_DGRAM) => {
            // Unix domain datagram socket
            knet::Socket::Unix(Box::new(UnixDomainSocket::new(DgramTransport::new(pid))))
        }
        (AF_NETLINK, SOCK_RAW) | (AF_NETLINK, SOCK_DGRAM) => {
            if proto > i32::MAX as u32 {
                return Err(KError::from(LinuxError::EPROTONOSUPPORT));
            }
            knet::Socket::Netlink(Box::new(NetlinkSocket::new(proto as i32)))
        }
        #[cfg(feature = "vsock")]
        (AF_VSOCK, SOCK_STREAM) => {
            // Virtio socket (hypervisor communication)
            knet::Socket::Vsock(Box::new(VsockSocket::new(VsockStreamTransport::new())))
        }
        (AF_INET, _) | (AF_UNIX, _) | (AF_VSOCK, _) | (AF_NETLINK, _) => {
            // Socket type not supported for this domain
            warn!("Unsupported socket type: domain: {domain}, ty: {ty}");
            return Err(KError::from(LinuxError::ESOCKTNOSUPPORT));
        }
        _ => {
            // Address family not supported
            return Err(KError::from(LinuxError::EAFNOSUPPORT));
        }
    };
    let socket = Socket(socket);

    if raw_ty & O_NONBLOCK != 0 {
        socket.set_nonblocking(true)?;
    }
    let cloexec = raw_ty & O_CLOEXEC != 0;

    kthread::current_resources()
        .add_file_like(Arc::new(socket), cloexec)
        .map(|fd| fd as isize)
}

/// Bind a socket to a local address
pub fn sys_bind(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> KResult<isize> {
    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;

    kthread::current_resources()
        .get_file_like_as::<Socket>(fd)?
        .bind(addr)?;

    Ok(0)
}

/// Initiate a connection to a remote address
pub fn sys_connect(fd: i32, addr: UserConstPtr<sockaddr>, addrlen: u32) -> KResult<isize> {
    let addr = SocketAddrEx::read_from_user(addr, addrlen)?;

    kthread::current_resources()
        .get_file_like_as::<Socket>(fd)?
        .connect(addr)
        .map_err(|e| {
            if e == KError::WouldBlock {
                KError::InProgress
            } else {
                e
            }
        })?;

    Ok(0)
}

/// Mark a socket as ready to accept incoming connections
pub fn sys_listen(fd: i32, backlog: i32) -> KResult<isize> {
    if backlog < 0 && backlog != -1 {
        return Err(KError::InvalidInput);
    }

    kthread::current_resources()
        .get_file_like_as::<Socket>(fd)?
        .listen()?;

    Ok(0)
}

/// Accept an incoming connection on a listening socket
pub fn sys_accept(fd: i32, addr: UserPtr<sockaddr>, addrlen: UserPtr<socklen_t>) -> KResult<isize> {
    sys_accept4(fd, addr, addrlen, 0)
}

/// Accept an incoming connection with additional flags (CLOEXEC, NONBLOCK)
pub fn sys_accept4(
    fd: i32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
    flags: u32,
) -> KResult<isize> {
    let cloexec = flags & O_CLOEXEC != 0;

    let socket = kthread::current_resources().get_file_like_as::<Socket>(fd)?;
    let socket = Socket(socket.accept()?);
    if flags & O_NONBLOCK != 0 {
        socket.set_nonblocking(true)?;
    }

    let remote_addr = socket.peer_addr()?;
    let fd = kthread::current_resources()
        .add_file_like(Arc::new(socket), cloexec)
        .map(|fd| fd as isize)?;

    if !addr.is_null() {
        remote_addr.write_to_user(addr, addrlen.get_as_mut()?)?;
    }

    Ok(fd)
}

/// Shut down all or part of a full-duplex connection
pub fn sys_shutdown(fd: i32, how: u32) -> KResult<isize> {
    let socket = kthread::current_resources().get_file_like_as::<Socket>(fd)?;
    let how = match how {
        SHUT_RD => Shutdown::Read,
        SHUT_WR => Shutdown::Write,
        SHUT_RDWR => Shutdown::Both,
        _ => return Err(KError::InvalidInput),
    };
    socket.shutdown(how).map(|_| 0)
}

/// Create a pair of connected sockets (Unix domain only)
pub fn sys_socketpair(
    domain: u32,
    raw_ty: u32,
    _proto: u32,
    fds: UserPtr<[i32; 2]>,
) -> KResult<isize> {
    let ty = raw_ty & 0xFF;

    if domain != AF_UNIX {
        return Err(KError::from(LinuxError::EAFNOSUPPORT));
    }

    let pid = kthread::current_thread().pid();
    let (sock1, sock2) = match ty {
        SOCK_STREAM => {
            let (sock1, sock2) = StreamTransport::new_pair(pid);
            (UnixDomainSocket::new(sock1), UnixDomainSocket::new(sock2))
        }
        SOCK_DGRAM | SOCK_SEQPACKET => {
            let (sock1, sock2) = DgramTransport::new_pair(pid);
            (UnixDomainSocket::new(sock1), UnixDomainSocket::new(sock2))
        }
        _ => {
            warn!("Unsupported socketpair type: {ty}");
            return Err(KError::from(LinuxError::ESOCKTNOSUPPORT));
        }
    };
    let sock1 = Socket(knet::Socket::Unix(Box::new(sock1)));
    let sock2 = Socket(knet::Socket::Unix(Box::new(sock2)));

    if raw_ty & O_NONBLOCK != 0 {
        sock1.set_nonblocking(true)?;
        sock2.set_nonblocking(true)?;
    }
    let cloexec = raw_ty & O_CLOEXEC != 0;

    *fds.get_as_mut()? = [
        kthread::current_resources().add_file_like(Arc::new(sock1), cloexec)?,
        kthread::current_resources().add_file_like(Arc::new(sock2), cloexec)?,
    ];
    Ok(0)
}
