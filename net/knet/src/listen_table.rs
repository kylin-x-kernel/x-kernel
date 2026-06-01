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

/// The result of accepting a TCP connection from the listen table.
///
/// Contains the socket handle and the local/remote endpoints
/// needed to construct an accepted [`TcpSocket`].
#[derive(Clone, Copy)]
pub(crate) struct AcceptedTcp {
    pub(crate) handle: SocketHandle,
    pub(crate) local_endpoint: IpEndpoint,
    pub(crate) remote_endpoint: IpEndpoint,
}

struct PendingConn {
    accepted: AcceptedTcp,
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

    pub fn can_accept(&self, endpoint: IpListenEndpoint, sockets: &SocketSet<'_>) -> KResult<bool> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let entry = entry.lock();
        Ok(entry
            .accept_queue
            .iter()
            .any(|conn| is_acceptable_in_socket_set(conn.accepted.handle, sockets)))
    }

    pub fn register_accept_waker(
        &self,
        endpoint: IpListenEndpoint,
        sockets: &SocketSet<'_>,
        waker: &core::task::Waker,
    ) -> KResult<()> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("register accept waker before listen");
            return Err(KError::InvalidInput);
        };

        let entry = entry.lock();
        entry.accept_poll.register(waker);
        if entry
            .accept_queue
            .iter()
            .any(|conn| is_acceptable_in_socket_set(conn.accepted.handle, sockets))
        {
            entry.accept_poll.wake();
        }
        Ok(())
    }

    pub fn accept(
        &self,
        endpoint: IpListenEndpoint,
        sockets: &mut SocketSet<'_>,
    ) -> KResult<AcceptedTcp> {
        let Some(entry) = self.get_entry(&endpoint) else {
            warn!("accept before listen");
            return Err(KError::InvalidInput);
        };

        let mut entry = entry.lock();
        let mut has_aborted_conn = refresh_entry_for_accept(&mut entry, sockets);

        loop {
            let conn = match entry.accept_queue.pop_front() {
                Some(conn) => conn,
                None if has_aborted_conn => return Err(KError::ConnectionAborted),
                None => return Err(KError::WouldBlock),
            };
            if is_acceptable_in_socket_set(conn.accepted.handle, sockets) {
                return Ok(conn.accepted);
            }

            warn!("accept failed: connection aborted");
            sockets.remove(conn.accepted.handle);
            has_aborted_conn = true;
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
            .any(|conn| conn.accepted.remote_endpoint == src && conn.accepted.local_endpoint == dst)
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
            accepted: AcceptedTcp {
                handle: dispatch_irq,
                local_endpoint: dst,
                remote_endpoint: src,
            },
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
        let mut handles = Vec::with_capacity(self.syn_queue.len() + self.accept_queue.len());
        while let Some(conn) = self.syn_queue.pop_front() {
            handles.push(conn.accepted.handle);
        }
        while let Some(conn) = self.accept_queue.pop_front() {
            handles.push(conn.accepted.handle);
        }
        handles
    }
}

fn refresh_entry_in_socket_set(entry: &mut ListenTableEntry, sockets: &mut SocketSet<'_>) -> bool {
    prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
    prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
    promote_ready_in_socket_set(&mut entry.syn_queue, &mut entry.accept_queue, sockets)
}

fn refresh_entry_for_accept(entry: &mut ListenTableEntry, sockets: &mut SocketSet<'_>) -> bool {
    prune_closed_in_socket_set(&mut entry.syn_queue, sockets);
    let pruned_accept_queue = prune_closed_in_socket_set(&mut entry.accept_queue, sockets);
    promote_ready_in_socket_set(&mut entry.syn_queue, &mut entry.accept_queue, sockets);
    pruned_accept_queue
}

fn prune_closed_in_socket_set(
    syn_queue: &mut VecDeque<PendingConn>,
    sockets: &mut SocketSet<'_>,
) -> bool {
    let mut closed = Vec::with_capacity(syn_queue.len() / 4);
    syn_queue.retain(|conn| {
        let keep = !is_discardable_in_socket_set(conn.accepted.handle, sockets);
        if !keep {
            closed.push(conn.accepted.handle);
        }
        keep
    });
    let pruned = !closed.is_empty();
    for handle in closed {
        sockets.remove(handle);
    }
    pruned
}

fn is_discardable_in_socket_set(handle: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    let socket = sockets.get::<tcp::Socket>(handle);
    match socket.state() {
        State::LastAck | State::TimeWait => true,
        State::Closed | State::FinWait1 | State::FinWait2 | State::Closing => {
            socket.recv_queue() == 0
        }
        State::Listen
        | State::SynSent
        | State::SynReceived
        | State::Established
        | State::CloseWait => false,
    }
}

fn is_acceptable_in_socket_set(handle: SocketHandle, sockets: &SocketSet<'_>) -> bool {
    is_acceptable_socket(sockets.get::<tcp::Socket>(handle))
}

fn is_acceptable_socket(socket: &tcp::Socket<'_>) -> bool {
    match socket.state() {
        State::Established | State::CloseWait => true,
        State::Closed | State::FinWait1 | State::FinWait2 | State::Closing => {
            socket.recv_queue() > 0
        }
        State::LastAck | State::TimeWait | State::Listen | State::SynSent | State::SynReceived => {
            false
        }
    }
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
        if is_acceptable_in_socket_set(conn.accepted.handle, sockets) {
            accept_queue.push_back(conn);
            moved = true;
        } else {
            syn_queue.push_back(conn);
        }
    }
    moved
}
