// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TCP listen table and backlog management.
use alloc::{collections::VecDeque, sync::Arc, vec, vec::Vec};

use hashbrown::HashMap;
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

struct ListenTableEntry {
    syn_queue: VecDeque<PendingConn>,
    accept_queue: VecDeque<PendingConn>,
    accept_poll: PollSet,
    backlog: usize,
    touched: bool,
    closed: bool,
}

struct PendingConn {
    src: IpEndpoint,
    dst: IpEndpoint,
    dispatch_irq: SocketHandle,
}

impl ListenTableEntry {
    /// Create a new listen table entry for the given endpoint.
    fn new(backlog: usize) -> Self {
        let backlog = backlog.clamp(1, LISTEN_QUEUE_SIZE);
        Self {
            syn_queue: VecDeque::with_capacity(backlog),
            accept_queue: VecDeque::with_capacity(backlog),
            accept_poll: PollSet::new(),
            backlog,
            touched: false,
            closed: false,
        }
    }
}

pub struct ListenTable {
    entries: Mutex<HashMap<IpListenEndpoint, ListenEntry>>,
}

type ListenEntry = Arc<Mutex<ListenTableEntry>>;

impl ListenTable {
    /// Create an empty listen table.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn can_listen(&self, endpoint: IpListenEndpoint) -> bool {
        let entries = self.entries.lock();
        !Self::listen_conflicts(&entries, endpoint)
    }

    pub fn listen(&self, listen_endpoint: IpListenEndpoint, backlog: usize) -> KResult {
        let port = listen_endpoint.port;
        assert_ne!(port, 0);

        let mut entries = self.entries.lock();
        if Self::listen_conflicts(&entries, listen_endpoint) {
            warn!("socket already listening on {listen_endpoint:?}");
            return Err(KError::AddrInUse);
        }

        entries.insert(
            listen_endpoint,
            Arc::new(Mutex::new(ListenTableEntry::new(backlog))),
        );
        Ok(())
    }

    pub fn unlisten(&self, endpoint: IpListenEndpoint) {
        let entry = {
            let mut entries = self.entries.lock();
            entries.remove(&endpoint)
        };

        let Some(entry) = entry else {
            return;
        };

        let handles = {
            let mut entry = entry.lock();
            entry.closed = true;
            entry.drain_handles()
        };

        for handle in handles {
            SOCKET_SET.remove(handle);
        }
    }

    pub fn can_accept(&self, endpoint: IpListenEndpoint) -> KResult<bool> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let entry = entry.lock();
        Ok(!entry.accept_queue.is_empty())
    }

    pub fn register_accept_waker(
        &self,
        endpoint: IpListenEndpoint,
        waker: &core::task::Waker,
    ) -> KResult<()> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("register accept waker before listen");
            return Err(KError::InvalidInput);
        };

        let entry = entry.lock();
        entry.accept_poll.register(waker);
        if !entry.accept_queue.is_empty() {
            entry.accept_poll.wake();
        }
        Ok(())
    }

    pub fn accept(&self, endpoint: IpListenEndpoint) -> KResult<SocketHandle> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let mut entry = entry.lock();

        loop {
            let conn = entry.accept_queue.pop_front().ok_or(KError::WouldBlock)?;
            if !is_closed(conn.dispatch_irq) {
                return Ok(conn.dispatch_irq);
            }

            warn!("accept failed: connection reset");
            SOCKET_SET.remove(conn.dispatch_irq);
        }
    }

    pub fn note_tcp_packet(&self, dst: IpEndpoint) {
        let Some((_, entry)) = self.lookup_entry(dst) else {
            return;
        };

        entry.lock().touched = true;
    }

    pub fn wake_touched_acceptors(&self, sockets: &mut SocketSet<'_>) {
        let entries = {
            let entries = self.entries.lock();
            entries.values().cloned().collect::<Vec<_>>()
        };

        for entry in entries {
            let mut entry = entry.lock();
            if entry.closed || !entry.touched {
                continue;
            }
            entry.touched = false;
            if refresh_entry_in_socket_set(&mut entry, sockets) {
                entry.accept_poll.wake();
            }
        }
    }

    pub fn incoming_tcp_packet(
        &self,
        src: IpEndpoint,
        dst: IpEndpoint,
        sockets: &mut SocketSet<'_>,
    ) {
        let Some((endpoint, entry)) = self.lookup_entry(dst) else {
            return;
        };

        let mut entry = entry.lock();
        if entry.closed {
            return;
        }

        prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
        prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
        if entry
            .syn_queue
            .iter()
            .chain(entry.accept_queue.iter())
            .any(|conn| conn.src == src && conn.dst == dst)
        {
            return;
        }
        // TODO(mivik): accept address check
        if entry.syn_queue.len() + entry.accept_queue.len() >= entry.backlog {
            warn!("listen backlog overflow!");
            return;
        }

        let mut socket = smoltcp::socket::tcp::Socket::new(
            SocketBuffer::new(vec![0; TCP_RX_BUF_LEN]),
            SocketBuffer::new(vec![0; TCP_TX_BUF_LEN]),
        );
        if let Err(err) = socket.listen(IpListenEndpoint {
            addr: Some(dst.addr),
            port: dst.port,
        }) {
            warn!("Failed to listen on {}: {:?}", endpoint, err);
            return;
        }
        let dispatch_irq = sockets.add(socket);
        entry.syn_queue.push_back(PendingConn {
            src,
            dst,
            dispatch_irq,
        });
    }

    fn get_entry(&self, endpoint: &IpListenEndpoint) -> Option<ListenEntry> {
        let entries = self.entries.lock();
        entries.get(endpoint).cloned()
    }

    fn lookup_entry(&self, dst: IpEndpoint) -> Option<(IpListenEndpoint, ListenEntry)> {
        let entries = self.entries.lock();
        Self::lookup_endpoint(&entries, dst).and_then(|endpoint| {
            entries
                .get(&endpoint)
                .cloned()
                .map(|entry| (endpoint, entry))
        })
    }

    fn lookup_endpoint(
        entries: &HashMap<IpListenEndpoint, ListenEntry>,
        dst: IpEndpoint,
    ) -> Option<IpListenEndpoint> {
        // This only selects a candidate endpoint; callers must re-check the entry
        // after taking the listen table lock.
        let exact = IpListenEndpoint {
            addr: Some(dst.addr),
            port: dst.port,
        };
        if entries.contains_key(&exact) {
            return Some(exact);
        }

        let wildcard = IpListenEndpoint {
            addr: None,
            port: dst.port,
        };
        entries.contains_key(&wildcard).then_some(wildcard)
    }

    fn listen_conflicts(
        entries: &HashMap<IpListenEndpoint, ListenEntry>,
        endpoint: IpListenEndpoint,
    ) -> bool {
        entries.keys().any(|old| {
            old.port == endpoint.port
                && (old.addr.is_none() || endpoint.addr.is_none() || old.addr == endpoint.addr)
        })
    }
}

impl ListenTableEntry {
    fn drain_handles(&mut self) -> Vec<SocketHandle> {
        let mut handles = Vec::new();
        while let Some(conn) = self.syn_queue.pop_front() {
            handles.push(conn.dispatch_irq);
        }
        while let Some(conn) = self.accept_queue.pop_front() {
            handles.push(conn.dispatch_irq);
        }
        handles
    }
}

fn is_closed(dispatch_irq: SocketHandle) -> bool {
    SOCKET_SET.with_socket::<tcp::Socket, _, _>(dispatch_irq, |socket| {
        matches!(socket.state(), State::Closed)
    })
}

fn refresh_entry_in_socket_set(entry: &mut ListenTableEntry, sockets: &mut SocketSet<'_>) -> bool {
    prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
    prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
    promote_ready_in_socket_set(&mut entry.syn_queue, &mut entry.accept_queue, sockets)
}

fn prune_closed_in_socket_set(syn_queue: &mut VecDeque<PendingConn>, sockets: &mut SocketSet<'_>) {
    let mut closed = Vec::new();
    syn_queue.retain(|conn| {
        let keep = !is_closed_in_socket_set(conn.dispatch_irq, sockets);
        if !keep {
            closed.push(conn.dispatch_irq);
        }
        keep
    });
    for dispatch_irq in closed {
        sockets.remove(dispatch_irq);
    }
}

fn is_closed_in_socket_set(dispatch_irq: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    matches!(
        sockets.get::<tcp::Socket>(dispatch_irq).state(),
        State::Closed
    )
}

fn is_connected_in_socket_set(dispatch_irq: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    matches!(
        sockets.get::<tcp::Socket>(dispatch_irq).state(),
        State::Established | State::CloseWait
    )
}

fn promote_ready_in_socket_set(
    syn_queue: &mut VecDeque<PendingConn>,
    accept_queue: &mut VecDeque<PendingConn>,
    sockets: &SocketSet<'_>,
) -> bool {
    let mut moved = false;
    let len = syn_queue.len();
    for _ in 0..len {
        let Some(conn) = syn_queue.pop_front() else {
            break;
        };
        if is_connected_in_socket_set(conn.dispatch_irq, sockets) {
            accept_queue.push_back(conn);
            moved = true;
        } else {
            syn_queue.push_back(conn);
        }
    }
    moved
}
