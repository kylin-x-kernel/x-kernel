// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP listen table and backlog management.
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec};
use core::ops::DerefMut;

use kerrno::{KError, KResult};
use kpoll::PollSet;
use ksync::Mutex;
use smoltcp::{
    iface::{SocketHandle, SocketSet},
    socket::tcp::{self, SocketBuffer, State},
    wire::{IpEndpoint, IpListenEndpoint},
};

use crate::{
    SOCKET_SET,
    consts::{LISTEN_QUEUE_SIZE, TCP_RX_BUF_LEN, TCP_TX_BUF_LEN},
};

const PORT_NUM: usize = 65536;

struct ListenTableEntry {
    listen_endpoint: IpListenEndpoint,
    syn_queue: VecDeque<PendingConn>,
    accept_queue: VecDeque<PendingConn>,
    accept_poll: PollSet,
}

struct PendingConn {
    src: IpEndpoint,
    dst: IpEndpoint,
    dispatch_irq: SocketHandle,
}

impl ListenTableEntry {
    /// Create a new listen table entry for the given endpoint.
    pub fn new(listen_endpoint: IpListenEndpoint) -> Self {
        Self {
            listen_endpoint,
            syn_queue: VecDeque::with_capacity(LISTEN_QUEUE_SIZE),
            accept_queue: VecDeque::with_capacity(LISTEN_QUEUE_SIZE),
            accept_poll: PollSet::new(),
        }
    }
}

impl Drop for ListenTableEntry {
    fn drop(&mut self) {
        for conn in &self.syn_queue {
            SOCKET_SET.remove(conn.dispatch_irq);
        }
        for conn in &self.accept_queue {
            SOCKET_SET.remove(conn.dispatch_irq);
        }
    }
}

pub struct ListenTable {
    tcp: TcpListenTable,
}

type TcpListenTable = Box<[Arc<Mutex<Option<Box<ListenTableEntry>>>>]>;

impl ListenTable {
    /// Create an empty listen table.
    pub fn new() -> Self {
        let tcp = unsafe {
            let mut buf = Box::new_uninit_slice(PORT_NUM);
            for i in 0..PORT_NUM {
                buf[i].write(Arc::default());
            }
            buf.assume_init()
        };
        Self { tcp }
    }

    pub fn can_listen(&self, port: u16) -> bool {
        self.tcp[port as usize].lock().is_none()
    }

    pub fn listen(&self, listen_endpoint: IpListenEndpoint) -> KResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);
        let mut entry = self.tcp[port as usize].lock();
        if entry.is_none() {
            *entry = Some(Box::new(ListenTableEntry::new(listen_endpoint)));
            Ok(())
        } else {
            warn!("socket already listening on port {port}");
            Err(KError::AddrInUse)
        }
    }

    pub fn unlisten(&self, port: u16) {
        *self.tcp[port as usize].lock() = None;
    }

    fn listen_entry(&self, port: u16) -> Arc<Mutex<Option<Box<ListenTableEntry>>>> {
        self.tcp[port as usize].clone()
    }

    pub fn can_accept(&self, port: u16) -> KResult<bool> {
        if let Some(entry) = self.listen_entry(port).lock().as_mut() {
            prune_closed(&mut entry.syn_queue);
            prune_closed(&mut entry.accept_queue);
            promote_ready(&mut entry.syn_queue, &mut entry.accept_queue);
            Ok(!entry.accept_queue.is_empty())
        } else {
            warn!("accept before listen");
            Err(KError::InvalidInput)
        }
    }

    pub fn register_accept_waker(&self, port: u16, waker: &core::task::Waker) -> KResult<()> {
        if let Some(entry) = self.listen_entry(port).lock().as_ref() {
            entry.accept_poll.register(waker);
            Ok(())
        } else {
            warn!("register accept waker before listen");
            Err(KError::InvalidInput)
        }
    }

    pub fn accept(&self, port: u16) -> KResult<SocketHandle> {
        let entry = self.listen_entry(port);
        let mut table = entry.lock();
        let Some(entry) = table.deref_mut() else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        prune_closed(&mut entry.syn_queue);
        prune_closed(&mut entry.accept_queue);
        promote_ready(&mut entry.syn_queue, &mut entry.accept_queue);
        let conn = entry.accept_queue.pop_front().ok_or(KError::WouldBlock)?;
        // If the connection is reset, return ConnectionReset error
        // Otherwise, return the dispatch_irq and the address tuple
        if is_closed(conn.dispatch_irq) {
            warn!("accept failed: connection reset");
            SOCKET_SET.remove(conn.dispatch_irq);
            Err(KError::ConnectionReset)
        } else {
            Ok(conn.dispatch_irq)
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        if let Some(entry) = self.listen_entry(dst.port).lock().deref_mut() {
            prune_closed(&mut entry.syn_queue);
            prune_closed(&mut entry.accept_queue);
            if entry
                .syn_queue
                .iter()
                .chain(entry.accept_queue.iter())
                .any(|conn| conn.src == src && conn.dst == dst)
            {
                return;
            }
            // TODO(mivik): accept address check
            if entry.syn_queue.len() + entry.accept_queue.len() >= LISTEN_QUEUE_SIZE {
                // SYN queue is full, drop the packet
                warn!("SYN queue overflow!");
                return;
            }

            let mut socket = smoltcp::socket::tcp::Socket::new(
                SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
                SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
            );
            if let Err(err) = socket.listen(IpListenEndpoint {
                addr: None,
                port: dst.port,
            }) {
                warn!("Failed to listen on {}: {:?}", entry.listen_endpoint, err);
                return;
            }
            let dispatch_irq = sockets.add(socket);
            entry.syn_queue.push_back(PendingConn {
                src,
                dst,
                dispatch_irq,
            });
        }
    }

    pub fn wake_ready_acceptors(&self) {
        for entry in self.tcp.iter() {
            let mut guard = entry.lock();
            let Some(entry) = guard.as_mut() else {
                continue;
            };
            prune_closed(&mut entry.syn_queue);
            prune_closed(&mut entry.accept_queue);
            if promote_ready(&mut entry.syn_queue, &mut entry.accept_queue) {
                entry.accept_poll.wake();
            }
        }
    }
}

fn is_connected(dispatch_irq: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(dispatch_irq, |socket| {
        matches!(socket.state(), State::Established | State::CloseWait)
    })
}

fn is_closed(dispatch_irq: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(dispatch_irq, |socket| {
        matches!(socket.state(), State::Closed)
    })
}

fn prune_closed(syn_queue: &mut VecDeque<PendingConn>) {
    let len = syn_queue.len();
    for _ in 0..len {
        let conn = syn_queue.pop_front().unwrap();
        if is_closed(conn.dispatch_irq) {
            SOCKET_SET.remove(conn.dispatch_irq);
        } else {
            syn_queue.push_back(conn);
        }
    }
}

fn promote_ready(
    syn_queue: &mut VecDeque<PendingConn>,
    accept_queue: &mut VecDeque<PendingConn>,
) -> bool {
    let mut moved = false;
    let len = syn_queue.len();
    for _ in 0..len {
        let conn = syn_queue.pop_front().unwrap();
        if is_connected(conn.dispatch_irq) {
            accept_queue.push_back(conn);
            moved = true;
        } else {
            syn_queue.push_back(conn);
        }
    }
    moved
}
