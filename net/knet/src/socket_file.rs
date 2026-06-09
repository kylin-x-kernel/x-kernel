// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File-like interface for network sockets.

use alloc::{borrow::Cow, format, sync::Arc};
use core::{ffi::c_int, task::Context};

use kerrno::{KError, KResult};
use kfd::{FdTable, FileLike, IoDst, IoSrc, Kstat};
use kpoll::{IoEvents, Pollable};
use ksync::RwLock;
use linux_raw_sys::general::{O_RDWR, S_IFSOCK};

use crate::{
    RecvOptions, SendOptions, Socket, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

impl FileLike for Socket {
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        self.recv(dst, RecvOptions::default())
    }

    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        self.send(src, SendOptions::default())
    }

    fn stat(&self) -> KResult<Kstat> {
        // TODO(mivik): implement stat for sockets
        Ok(Kstat {
            mode: S_IFSOCK | 0o777u32,
            blksize: 4096,
            ..Default::default()
        })
    }

    fn nonblocking(&self) -> bool {
        let mut result = false;
        self.get_option(GetSocketOption::NonBlocking(&mut result))
            .unwrap();
        result
    }

    fn set_nonblocking(&self, nonblocking: bool) -> KResult<()> {
        self.set_option(SetSocketOption::NonBlocking(&nonblocking))
    }

    fn path(&self) -> Cow<'_, str> {
        format!("socket:[{}]", self as *const _ as usize).into()
    }

    fn open_flags(&self) -> u32 {
        O_RDWR
    }

    fn from_fd(fd_table: &RwLock<FdTable>, fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        fd_table
            .read()
            .get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::NotASocket)
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

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
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
