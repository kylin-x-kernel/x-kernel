// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket set wrapper utilities.

use alloc::{vec, vec::Vec};
use core::ops::{Deref, DerefMut};

use event_listener::Event;
use ksync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::{AnySocket, tcp},
    time::{Duration, Instant},
};

const ORPHAN_FIN_WAIT2_TIMEOUT: Duration = Duration::from_secs(60);

fn orphan_fin_wait2_deadline(
    state: tcp::State,
    current: Instant,
    deadline: Option<Instant>,
) -> Option<Instant> {
    (state == tcp::State::FinWait2).then(|| deadline.unwrap_or(current + ORPHAN_FIN_WAIT2_TIMEOUT))
}

struct DeferredTcpClose {
    handle: SocketHandle,
    fin_wait2_deadline: Option<Instant>,
}

pub(crate) struct SocketSetState<'a> {
    sockets: SocketSet<'a>,
    deferred_tcp_closes: Vec<DeferredTcpClose>,
}

impl SocketSetState<'_> {
    pub fn new() -> Self {
        Self {
            sockets: SocketSet::new(vec![]),
            deferred_tcp_closes: Vec::new(),
        }
    }

    pub fn defer_tcp_close(&mut self, handle: SocketHandle) {
        debug_assert!(
            self.deferred_tcp_closes
                .iter()
                .all(|entry| entry.handle != handle)
        );
        self.deferred_tcp_closes.push(DeferredTcpClose {
            handle,
            fin_wait2_deadline: None,
        });
    }

    pub fn deferred_tcp_close_deadline(&self) -> Option<Instant> {
        self.deferred_tcp_closes
            .iter()
            .filter_map(|entry| entry.fin_wait2_deadline)
            .min()
    }

    pub fn reap_deferred_tcp_closes(&mut self, current: Instant) -> Option<Instant> {
        let Self {
            sockets,
            deferred_tcp_closes,
        } = self;
        let mut next_deadline = None;
        deferred_tcp_closes.retain_mut(|entry| {
            let socket = sockets.get::<tcp::Socket>(entry.handle);
            let state = socket.state();
            // smoltcp keeps the tuple until a CLOSED socket's pending RST has
            // been emitted. The protocol object must remain reachable until
            // that packet has actually been dispatched.
            if state == tcp::State::Closed && socket.local_endpoint().is_none() {
                sockets.remove(entry.handle);
                return false;
            }

            entry.fin_wait2_deadline =
                orphan_fin_wait2_deadline(state, current, entry.fin_wait2_deadline);
            if let Some(deadline) = entry.fin_wait2_deadline {
                if current >= deadline {
                    sockets.remove(entry.handle);
                    return false;
                }
                next_deadline =
                    Some(next_deadline.map_or(deadline, |current: Instant| current.min(deadline)));
            }
            true
        });
        next_deadline
    }
}

impl<'a> Deref for SocketSetState<'a> {
    type Target = SocketSet<'a>;

    fn deref(&self) -> &Self::Target {
        &self.sockets
    }
}

impl DerefMut for SocketSetState<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.sockets
    }
}

pub(crate) struct SocketSetWrapper<'a> {
    pub inner: Mutex<SocketSetState<'a>>,
    pub new_socket: Event,
}

impl<'a> SocketSetWrapper<'a> {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(SocketSetState::new()),
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

#[cfg(unittest)]
mod tests {
    use smoltcp::{socket::tcp, time::Instant};
    use unittest::{assert_eq, def_test};

    use super::{ORPHAN_FIN_WAIT2_TIMEOUT, orphan_fin_wait2_deadline};

    #[def_test]
    fn only_orphan_fin_wait2_gets_a_cleanup_deadline() {
        let current = Instant::from_millis(10);

        for state in [
            tcp::State::FinWait1,
            tcp::State::Closing,
            tcp::State::LastAck,
            tcp::State::TimeWait,
        ] {
            assert_eq!(orphan_fin_wait2_deadline(state, current, None), None);
        }
        assert_eq!(
            orphan_fin_wait2_deadline(tcp::State::FinWait2, current, None),
            Some(current + ORPHAN_FIN_WAIT2_TIMEOUT)
        );
    }

    #[def_test]
    fn orphan_fin_wait2_keeps_its_original_cleanup_deadline() {
        let current = Instant::from_millis(10);
        let deadline = current + ORPHAN_FIN_WAIT2_TIMEOUT;

        assert_eq!(
            orphan_fin_wait2_deadline(
                tcp::State::FinWait2,
                Instant::from_millis(20),
                Some(deadline)
            ),
            Some(deadline)
        );
    }
}
