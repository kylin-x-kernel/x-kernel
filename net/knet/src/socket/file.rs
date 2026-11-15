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
use kprocess::AsThread;
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
    AnonInodeFs::global().get_socket_file(
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
        let mut options = SendOptions {
            opener_cred: Some(file.cred().as_ref().clone()),
            ..SendOptions::default()
        };
        if file.is_nonblocking() {
            options.flags |= SendFlags::DONT_WAIT;
        }
        match socket.as_ref() {
            Socket::Netlink(_) => match ktask::current().try_as_thread() {
                Some(_) => {
                    options.sender_credentials = current_sender_credentials();
                    let cred = kprocess::current_cred();
                    socket.send_with_cred(src, options, &cred)
                }
                None => socket.send(src, options),
            },
            Socket::Unix(unix) if unix.should_attach_sender_credentials() => {
                options.sender_credentials = current_sender_credentials();
                socket.send(src, options)
            }
            _ => socket.send(src, options),
        }
    }
}

impl FileOperations for SocketFileOps {
    fn fsync(
        &self,
        _file: &VfsFile,
        _range: core::ops::Range<u64>,
        _data_only: bool,
    ) -> VfsResult<()> {
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

    fn ioctl(&self, file: &VfsFile, _cred: &Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        // Linux `sock_do_ioctl` forwards these to the protocol's ioctl
        // (`unix_ioctl`, `tcp_ioctl`, `udp_ioctl`), which copies one int
        // result to the caller. `SIOCINQ` shares `FIONREAD`'s number
        // (include/uapi/asm-generic/ioctls.h). Everything else keeps the
        // generic non-tty rejection, which is what an unimplemented socket
        // ioctl folds to through `sock_ioctl`.
        const SIOCOUTQ: u32 = 0x5411;
        const SIOCINQ: u32 = 0x541B;
        if cmd != SIOCOUTQ && cmd != SIOCINQ {
            return Err(KError::NotATty);
        }
        let socket = Self::socket(file)?;
        // `unix_ioctl` answers SIOCOUTQ from `unix_outq_len` unconditionally,
        // while SIOCINQ goes through `unix_inq_len`, which refuses a listening
        // socket with EINVAL. `tcp_ioctl` makes the same split.
        if cmd == SIOCINQ && socket.is_listening() {
            return Err(KError::InvalidInput);
        }
        // A transport that cannot see its own queue answers ENOTTY: a zero
        // reads as "nothing is queued" to a caller such as glibc's resolver,
        // which abandons a lookup that has a reply waiting.
        let Some(lengths) = socket.queue_lengths()? else {
            return Err(KError::NotATty);
        };
        let amount = if cmd == SIOCOUTQ {
            lengths.outq
        } else {
            lengths.inq
        };
        osvm::write_vm_bytes(arg as *mut u8, &amount.to_ne_bytes())?;
        Ok(0)
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
