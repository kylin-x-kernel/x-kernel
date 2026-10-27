// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Open file operations for network sockets.

use alloc::{format, sync::Arc};

use anon_inodefs::AnonInodeFs;
use iov_iter::{IovIterDest, IovIterSource};
use kcred::Cred;
use kerrno::{KError, KResult};
use kio::{IoBuf, IoBufMut, Read, Write};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kvfs::{FMode, FileOperations, Kiocb, OpenFlags, VfsFile, VfsResult};

use crate::{
    RecvFlags, RecvOptions, SendFlags, SendOptions, Socket, SocketOps,
    unix::current_sender_credentials,
};

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

struct SocketInput<'a, 'b>(&'a mut IovIterSource<'b>);

impl Read for SocketInput<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> KResult<usize> {
        self.0.copy_from_iter(buf)
    }
}

impl IoBuf for SocketInput<'_, '_> {
    fn remaining(&self) -> usize {
        self.0.count()
    }
}

struct SocketOutput<'a, 'b>(&'a mut IovIterDest<'b>);

impl Write for SocketOutput<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> KResult<usize> {
        self.0.copy_to_iter(buf)
    }

    fn flush(&mut self) -> KResult<()> {
        Ok(())
    }
}

impl IoBufMut for SocketOutput<'_, '_> {
    fn remaining_mut(&self) -> usize {
        self.0.count()
    }
}

impl SocketFileOps {
    fn socket(file: &VfsFile) -> VfsResult<Arc<Socket>> {
        sock_from_file(file)
    }

    fn recv(file: &VfsFile, dst: impl Write + IoBufMut) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let mut options = RecvOptions::default();
        if file.is_nonblocking() {
            options.flags |= RecvFlags::DONT_WAIT;
        }
        socket.recv(dst, options)
    }

    fn send(file: &VfsFile, src: impl Read + IoBuf) -> VfsResult<usize> {
        let socket = Self::socket(file)?;
        let mut options = SendOptions::default();
        if file.is_nonblocking() {
            options.flags |= SendFlags::DONT_WAIT;
        }
        match socket.as_ref() {
            Socket::Netlink(_) => {
                let cred = kprocess::current_cred();
                socket.send_with_cred(src, options, &cred)
            }
            Socket::Unix(unix) if unix.is_datagram() => {
                options.unix_sender_credentials = Some(current_sender_credentials());
                socket.send(src, options)
            }
            _ => socket.send(src, options),
        }
    }
}

impl FileOperations for SocketFileOps {
    fn fsync(&self, _file: &VfsFile, _data_only: bool) -> VfsResult<()> {
        Err(kvfs::VfsError::InvalidInput)
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Self::recv(file, buf)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        Self::send(file, buf)
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        if iter.count() == 0 {
            return Ok(0);
        }
        Self::recv(iocb.file(), SocketOutput(iter))
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        // One iterator request is one socket send, including an empty datagram.
        Self::send(iocb.file(), SocketInput(iter))
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Some(
            Self::socket(file)
                .map(|socket| socket.poll())
                .unwrap_or_else(|_| IoEvents::ERR),
        )
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

unittest::path_mod_test!(tests, "../../ut-tests/socket/file/tests.rs");

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
