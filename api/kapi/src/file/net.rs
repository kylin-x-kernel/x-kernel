// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket file wrapper for the VFS layer.

use alloc::{borrow::Cow, format, sync::Arc};
use core::{ffi::c_int, ops::Deref, task::Context};

use kerrno::{KError, KResult};
use knet::{
    SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};
use kpoll::{IoEvents, Pollable};
use linux_raw_sys::general::S_IFSOCK;

use super::{FileLike, Kstat};
use crate::file::{IoDst, IoSrc, get_file_like};

/// Socket wrapper providing file-like interface for network sockets.
///
/// This struct wraps the underlying kernel network socket and implements
/// the `FileLike` trait to provide standard file operations like read, write, and stat.
pub struct Socket(pub knet::Socket);

impl Deref for Socket {
    /// Provides transparent access to the underlying network socket.
    type Target = knet::Socket;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FileLike for Socket {
    /// Receives data from the socket.
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        self.recv(dst, knet::RecvOptions::default())
    }

    /// Sends data to the socket.
    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        self.send(src, knet::SendOptions::default())
    }

    /// Returns socket statistics.
    /// Note: Full socket stat implementation is not yet complete.
    fn stat(&self) -> KResult<Kstat> {
        // TODO(mivik): implement stat for sockets
        Ok(Kstat {
            mode: S_IFSOCK | 0o777u32, // rwxrwxrwx
            blksize: 4096,
            ..Default::default()
        })
    }

    /// Checks if the socket is in non-blocking mode.
    fn nonblocking(&self) -> bool {
        let mut result = false;
        self.get_option(GetSocketOption::NonBlocking(&mut result))
            .unwrap();
        result
    }

    /// Sets or clears the non-blocking mode for this socket.
    fn set_nonblocking(&self, nonblocking: bool) -> KResult<()> {
        self.0
            .set_option(SetSocketOption::NonBlocking(&nonblocking))
    }

    /// Returns a string representation of the socket address.
    fn path(&self) -> Cow<'_, str> {
        format!("socket:[{}]", self as *const _ as usize).into()
    }

    /// Converts a file descriptor to a socket reference.
    fn from_fd(fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::NotASocket)
    }
}
impl Pollable for Socket {
    /// Polls for available I/O events on this socket.
    fn poll(&self) -> IoEvents {
        self.0.poll()
    }

    /// Registers the socket for polling with the given context and events.
    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.0.register(context, events);
    }
}
