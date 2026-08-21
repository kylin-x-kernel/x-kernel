// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open file operations for network sockets.

use alloc::{format, sync::Arc};

use anon_inodefs::AnonInodeFs;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile, VfsResult};

use crate::{RecvFlags, RecvOptions, SendFlags, SendOptions, Socket, SocketOps};

/// Wraps a socket in a VFS file and captures `cred` as its open credential.
pub fn sock_alloc_file(socket: Socket, flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
    let flags = OpenFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    let socket = Arc::new(socket);
    let name = format!("socket:[{}]", socket.as_ref() as *const _ as usize);
    AnonInodeFs::global().get_file(
        &name,
        Arc::new(SocketFileOps),
        socket,
        FMode::READ | FMode::WRITE | FMode::STREAM,
        flags,
        cred,
    )
}

pub fn sock_from_file(file: &VfsFile) -> KResult<Arc<Socket>> {
    file.private_data_get::<Socket>().ok_or(KError::NotASocket)
}

struct SocketFileOps;

impl SocketFileOps {
    fn socket(file: &VfsFile) -> VfsResult<Arc<Socket>> {
        sock_from_file(file)
    }
}

impl FileOperations for SocketFileOps {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let mut dst = buf;
        let mut options = RecvOptions::default();
        if file.is_nonblocking() {
            options.flags |= RecvFlags::DONT_WAIT;
        }
        socket.recv(&mut dst, options)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let src = buf;
        let mut options = SendOptions::default();
        if file.is_nonblocking() {
            options.flags |= SendFlags::DONT_WAIT;
        }
        match socket.as_ref() {
            Socket::Netlink(_) => {
                let cred = kprocess::current_cred();
                socket.send_with_cred(src, options, &cred)
            }
            _ => socket.send(src, options),
        }
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        Self::socket(file)
            .map(|socket| socket.poll())
            .unwrap_or_else(|_| IoEvents::ERR)
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(socket) = Self::socket(file) {
            socket.register(context, events)?;
        }
        Ok(())
    }
}

impl Pollable for Socket {
    fn poll(&self) -> IoEvents {
        match self {
            Socket::Tcp(tcp) => tcp.poll(),
            Socket::Udp(udp) => udp.poll(),
            Socket::Raw(raw) => raw.poll(),
            Socket::Unix(unix) => unix.poll(),
            Socket::Netlink(netlink) => netlink.poll(),
            Socket::Packet(packet) => packet.poll(),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.poll(),
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match self {
            Socket::Tcp(tcp) => tcp.register(context, events),
            Socket::Udp(udp) => udp.register(context, events),
            Socket::Raw(raw) => raw.register(context, events),
            Socket::Unix(unix) => unix.register(context, events),
            Socket::Netlink(netlink) => netlink.register(context, events),
            Socket::Packet(packet) => packet.register(context, events),
            #[cfg(feature = "vsock")]
            Socket::Vsock(vsock) => vsock.register(context, events),
        }
    }
}

#[cfg(unittest)]
mod socket_tests {
    use linux_raw_sys::general::S_IFSOCK;
    use unittest::def_test;

    /// Test S_IFSOCK constant
    #[def_test]
    fn test_socket_mode_constant() {
        assert_eq!(S_IFSOCK, 0o140000);
    }
}
