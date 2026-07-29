// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket set wrapper utilities.

use alloc::vec;

use event_listener::Event;
use ksync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::AnySocket,
};

pub(crate) struct SocketSetWrapper<'a> {
    pub inner: Mutex<SocketSet<'a>>,
    pub new_socket: Event,
}

impl<'a> SocketSetWrapper<'a> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SocketSet::new(vec![])),
            new_socket: Event::new(),
        }
    }

    pub fn add<T: AnySocket<'a>>(&self, socket: T) -> SocketHandle {
        let dispatch_irq = self.inner.lock().add(socket);
        self.new_socket.notify(1);
        dispatch_irq
    }

    pub fn with_socket_mut<T: AnySocket<'a>, R, F>(&self, dispatch_irq: SocketHandle, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        let mut set = self.inner.lock();
        let socket = set.get_mut(dispatch_irq);
        f(socket)
    }

    pub fn remove(&self, dispatch_irq: SocketHandle) {
        self.inner.lock().remove(dispatch_irq);
    }
}
