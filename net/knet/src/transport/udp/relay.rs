// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent nonblocking UDP relay socket.

use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use kerrno::{KError, KResult};

use super::UdpSocket;
use crate::{SERVICE, SocketAddrEx, SocketOps};

/// Persistent UDP relay socket for request/reply datagram flows.
pub struct UdpDatagramRelay {
    socket: UdpSocket,
}

impl UdpDatagramRelay {
    /// Creates a UDP relay socket with an ephemeral local port.
    pub fn new() -> KResult<Self> {
        Self::new_with_port(0)
    }

    /// Creates a UDP relay socket bound to `local_port`.
    pub fn new_with_port(local_port: u16) -> KResult<Self> {
        if !SERVICE.is_inited() {
            return Err(KError::OperationNotSupported);
        }

        let socket = UdpSocket::new();
        socket.bind(SocketAddrEx::Ip(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            local_port,
        )))?;
        Ok(Self { socket })
    }

    /// Sends a UDP datagram through this relay socket.
    pub fn send_to(&self, dst: SocketAddr, payload: &[u8]) -> KResult<usize> {
        self.socket.send_datagram_now(dst, payload)
    }

    /// Receives a UDP datagram without blocking.
    pub fn try_recv(&self, buf: &mut [u8]) -> KResult<Option<(usize, SocketAddr)>> {
        self.socket.recv_datagram_now(buf)
    }
}
