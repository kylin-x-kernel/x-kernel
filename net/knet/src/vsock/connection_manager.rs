// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Vsock connection manager.
#[cfg(feature = "vsock_tipc_bridge")]
use alloc::collections::{BTreeSet, VecDeque};
use alloc::{collections::BTreeMap, string::ToString, sync::Arc};
use core::sync::atomic::{AtomicU64, Ordering};

use kclass::{ClassDevice, prelude::*};
use kdevice::{Device, DeviceId, DeviceKind};
use kerrno::{KError, KResult, k_bail};
#[cfg(feature = "vsock_tipc_bridge")]
use klazy::Lazy;
use kpoll::{PollContext, PollRegisterError, PollSet};
use ksync::{Mutex, static_lock};
use ktask::{
    WaitQueue,
    future::{block_on, interruptible},
};
use ktime_types::TimeSpan;
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};

pub const VSOCK_RX_BUFFER_SIZE: usize = 128 * 1024; // 128KB receive buffer
const VSOCK_ACCEPT_QUEUE_SIZE: usize = 128; // accept queue size

/// connection states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Listening,
    Connecting,
    Connected,
    Closed,
}

/// Event delivered to the vsock-TIPC bridge for one of its owned connections.
#[cfg(feature = "vsock_tipc_bridge")]
#[derive(Debug)]
pub enum VsockBridgeEvent {
    /// The peer requested a new connection on a bridge-listened port.
    ConnectionRequest(VsockConnId),
    /// The peer accepted an outbound (reverse) bridge connection.
    Connected(VsockConnId),
    /// Data is available for a bridge connection.
    Received(VsockConnId, usize),
    /// The peer or transport closed a bridge connection.
    Disconnected(VsockConnId),
    /// The peer advertised new credit for a bridge connection.
    CreditUpdate(VsockConnId),
}

/// Connection
pub struct Connection {
    state: ConnectionState,
    local_addr: VsockAddr,
    peer_addr: Option<VsockAddr>,

    /// recv buffer read from driver
    rx_producer: HeapProd<u8>,
    rx_consumer: HeapCons<u8>,

    /// wait queues for tx due to InsufficientBufferSpaceInPeer
    tx_wait_queue: Arc<WaitQueue>,

    /// Waker lists
    rx_wakers: PollSet,
    connect_wakers: PollSet,

    /// closed flags
    rx_closed: bool,
    tx_closed: bool,

    /// Current connection info (address + credit) for outgoing packet headers.
    pub(crate) info: VsockConnectionInfo,
    /// True for connections owned by the vsock-TIPC bridge.
    #[allow(dead_code)]
    is_bridge: bool,
    /// `fwd_cnt` value included in the last credit update we sent to the peer.
    last_sent_fwd_cnt: u32,
}

impl Connection {
    fn new(
        conn_id: VsockConnId,
        local_addr: VsockAddr,
        peer_addr: Option<VsockAddr>,
        state: ConnectionState,
        is_bridge: bool,
    ) -> Self {
        let rb = HeapRb::<u8>::new(VSOCK_RX_BUFFER_SIZE);
        let (rx_producer, rx_consumer) = rb.split();
        Self {
            state,
            local_addr,
            peer_addr,
            rx_producer,
            rx_consumer,
            tx_wait_queue: Arc::new(WaitQueue::default()),
            rx_wakers: PollSet::new(),
            connect_wakers: PollSet::new(),
            rx_closed: false,
            tx_closed: false,
            info: VsockConnectionInfo {
                conn_id,
                buf_alloc: VSOCK_RX_BUFFER_SIZE as u32,
                fwd_cnt: 0,
                peer_buf_alloc: 0,
                peer_fwd_cnt: 0,
                tx_cnt: 0,
            },
            is_bridge,
            last_sent_fwd_cnt: 0,
        }
    }

    /// Register a waker for receive Events
    pub fn register_rx_poll(
        &mut self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.rx_wakers)
    }

    /// Register a waker for connect Events
    pub fn register_connect_poll(
        &mut self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.connect_wakers)
    }

    /// Get the free space in the receive buffer
    #[inline]
    pub fn rx_buffer_free(&self) -> usize {
        self.rx_producer.vacant_len()
    }

    /// Get the used space in the receive buffer
    #[inline]
    pub fn rx_buffer_used(&self) -> usize {
        self.rx_consumer.occupied_len()
    }

    /// push data into the receive buffer
    pub fn push_rx_data(&mut self, data: &[u8]) -> usize {
        let available = self.rx_buffer_free();
        let to_write = data.len().min(available);

        if to_write > 0 {
            let written = self.rx_producer.push_slice(&data[..to_write]);

            if written < data.len() {
                info!(
                    "Vsock connection {:?} rx buffer full, dropped {} bytes",
                    (self.local_addr, self.peer_addr),
                    data.len() - written
                );
            }
            written
        } else {
            info!(
                "Vsock connection {:?} rx buffer full, dropped {} bytes",
                (self.local_addr, self.peer_addr),
                data.len()
            );
            0
        }
    }

    #[inline]
    pub fn tx_wait_queue(&self) -> Arc<WaitQueue> {
        self.tx_wait_queue.clone()
    }

    #[inline]
    pub fn rx_slices(&self) -> (&[u8], &[u8]) {
        self.rx_consumer.as_slices()
    }

    #[inline]
    pub fn advance_rx_read(&mut self, count: usize) {
        // SAFETY: Callers compute `count` from bytes copied out of `rx_slices`
        // while holding the connection lock, so the advance stays within the
        // currently occupied receive buffer.
        unsafe {
            self.rx_consumer.advance_read_index(count);
        }
    }

    #[inline]
    pub fn add_tx_bytes(&mut self, count: usize) {
        self.info.tx_cnt = self.info.tx_cnt.wrapping_add(count as u32);
    }

    #[inline]
    pub fn wake_rx(&mut self) {
        self.rx_wakers.wake();
    }

    #[inline]
    pub fn wake_connect(&mut self) {
        self.connect_wakers.wake();
    }

    #[inline]
    pub fn local_addr(&self) -> VsockAddr {
        self.local_addr
    }

    #[inline]
    pub fn peer_addr(&self) -> Option<VsockAddr> {
        self.peer_addr
    }

    #[inline]
    pub fn set_state(&mut self, state: ConnectionState) {
        self.state = state;
    }

    #[inline]
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    #[inline]
    pub fn rx_closed(&self) -> bool {
        self.rx_closed
    }

    #[inline]
    pub fn tx_closed(&self) -> bool {
        self.tx_closed
    }

    #[inline]
    pub fn set_rx_closed(&mut self, closed: bool) {
        self.rx_closed = closed;
    }

    #[inline]
    pub fn set_tx_closed(&mut self, closed: bool) {
        self.tx_closed = closed;
    }
}

/// A fixed-size accept queue
pub struct AcceptQueue {
    producer: ringbuf::HeapProd<VsockConnId>,
    consumer: ringbuf::HeapCons<VsockConnId>,
}

impl AcceptQueue {
    pub fn new() -> Self {
        let rb = HeapRb::<VsockConnId>::new(VSOCK_ACCEPT_QUEUE_SIZE);
        let (producer, consumer) = rb.split();
        Self { producer, consumer }
    }

    pub fn is_empty(&self) -> bool {
        self.consumer.is_empty()
    }

    pub fn push(&mut self, conn_id: VsockConnId) -> KResult<()> {
        match self.producer.try_push(conn_id) {
            Ok(_) => Ok(()),
            Err(_) => k_bail!(ResourceBusy, "accept queue full"),
        }
    }

    pub fn pop(&mut self) -> Option<VsockConnId> {
        self.consumer.try_pop()
    }
}

/// listen queue
pub struct ListenQueue {
    pub accept_queue: AcceptQueue,
    pub wakers: PollSet,
}

impl ListenQueue {
    pub fn new() -> Self {
        Self {
            accept_queue: AcceptQueue::new(),
            wakers: PollSet::new(),
        }
    }

    pub fn wake(&mut self) {
        self.wakers.wake();
    }

    pub fn register_poll(
        &mut self,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError> {
        context.register(&self.wakers)
    }
}

/// Global connection manager
pub struct VsockConnectionManager {
    connections: BTreeMap<VsockConnId, Arc<Mutex<Connection>>>,
    listen_queues: BTreeMap<u32, Arc<Mutex<ListenQueue>>>,
    next_ephemeral_port: u32,
    raw_transport: Option<Arc<dyn VsockDevice + Send + Sync>>,
    #[cfg(feature = "vsock_tipc_bridge")]
    bridge_ports: BTreeSet<u32>,
    #[cfg(feature = "vsock_tipc_bridge")]
    bridge_events: VecDeque<VsockBridgeEvent>,
    guest_cid: u64,
}

impl VsockConnectionManager {
    const EPHEMERAL_PORT_END: u32 = 0xffff;
    const EPHEMERAL_PORT_START: u32 = 0xc000;

    pub const fn new() -> Self {
        Self {
            connections: BTreeMap::new(),
            listen_queues: BTreeMap::new(),
            next_ephemeral_port: Self::EPHEMERAL_PORT_START,
            raw_transport: None,
            #[cfg(feature = "vsock_tipc_bridge")]
            bridge_ports: BTreeSet::new(),
            #[cfg(feature = "vsock_tipc_bridge")]
            bridge_events: VecDeque::new(),
            guest_cid: 0,
        }
    }

    /// Install the raw transport and return the guest CID assigned by the host.
    pub fn set_raw_transport(&mut self, transport: Arc<dyn VsockDevice + Send + Sync>) -> u64 {
        self.guest_cid = transport.guest_cid();
        self.raw_transport = Some(transport);
        self.guest_cid
    }

    /// Returns the guest CID, or 0 if the transport has not been installed yet.
    pub fn guest_cid(&self) -> u64 {
        self.guest_cid
    }

    fn clone_raw_transport(&self) -> Option<Arc<dyn VsockDevice + Send + Sync>> {
        self.raw_transport.clone()
    }

    /// Detach the raw transport so that subsequent socket operations
    /// (connect, send, poll, etc.) return `BadState` rather than trying
    /// to use a device that has been unregistered.
    fn detach_raw_transport(&mut self) {
        self.raw_transport = None;
        self.guest_cid = 0;

        for conn in self.connections.values() {
            let mut c = conn.lock();
            c.set_state(ConnectionState::Closed);
            c.set_rx_closed(true);
            c.set_tx_closed(true);
            c.wake_rx();
            c.wake_connect();
            c.tx_wait_queue().notify_all(false);
        }

        for queue in self.listen_queues.values() {
            queue.lock().wake();
        }
        self.listen_queues.clear();

        #[cfg(feature = "vsock_tipc_bridge")]
        {
            self.bridge_events.clear();
            BRIDGE_EVENT_WAKERS.wake();
        }
    }

    /// Bridge-side API:
    /// register a bridge-listened port as being owned by the vsock-TIPC bridge.
    #[cfg(feature = "vsock_tipc_bridge")]
    pub fn listen_bridge_port(&mut self, port: u32) {
        self.bridge_ports.insert(port);
    }

    /// Returns whether the connection belongs to the bridge.
    #[cfg(feature = "vsock_tipc_bridge")]
    pub fn is_bridge_conn(&self, conn_id: VsockConnId) -> bool {
        self.bridge_ports.contains(&conn_id.local_port)
            || self
                .connections
                .get(&conn_id)
                .is_some_and(|c| c.lock().is_bridge)
    }

    /// Get listen queue from specified port
    pub fn get_listen_queue(&self, port: u32) -> Option<Arc<Mutex<ListenQueue>>> {
        self.listen_queues.get(&port).cloned()
    }

    /// allocate an ephemeral port
    pub fn allocate_port(&mut self) -> KResult<u32> {
        let start = self.next_ephemeral_port;
        loop {
            let port = self.next_ephemeral_port;
            self.next_ephemeral_port = if port >= Self::EPHEMERAL_PORT_END {
                Self::EPHEMERAL_PORT_START
            } else {
                port + 1
            };

            if !self.is_local_port_in_use(port) {
                return Ok(port);
            }

            if self.next_ephemeral_port == start {
                k_bail!(AddrInUse, "no available ports");
            }
        }
    }

    /// Returns whether a local port is already held by a listener, a
    /// connection, or the vsock-TIPC bridge.
    pub(crate) fn is_local_port_in_use(&self, port: u32) -> bool {
        #[cfg(feature = "vsock_tipc_bridge")]
        if self.bridge_ports.contains(&port) {
            return true;
        }
        self.listen_queues.contains_key(&port)
            || self.connections.keys().any(|id| id.local_port == port)
    }

    /// create a listen queue
    pub fn listen(&mut self, local_addr: VsockAddr) -> KResult<()> {
        if self.listen_queues.contains_key(&local_addr.port) {
            k_bail!(AddrInUse, "port already in use");
        }

        let queue = Arc::new(Mutex::new(ListenQueue::new()));
        self.listen_queues.insert(local_addr.port, queue);
        Ok(())
    }

    /// stop listening
    pub fn unlisten(&mut self, port: u32) {
        self.listen_queues.remove(&port);
        debug!("Vsock unlisten on port {}", port);
    }

    /// check if port accept
    pub fn can_accept(&self, port: u32) -> bool {
        self.listen_queues
            .get(&port)
            .map(|q| !q.lock().accept_queue.is_empty())
            .unwrap_or(false)
    }

    /// accept a connection
    pub fn accept(&mut self, port: u32) -> KResult<(VsockConnId, VsockAddr)> {
        let queue = self.listen_queues.get(&port).ok_or(KError::InvalidInput)?;

        let conn_id = queue.lock().accept_queue.pop().ok_or(KError::WouldBlock)?;

        let conn = self.connections.get(&conn_id).ok_or(KError::NotFound)?;

        let peer_addr = conn.lock().peer_addr.ok_or(KError::NotFound)?;

        debug!("Accepted connection: {:?} from {:?}", conn_id, peer_addr);
        Ok((conn_id, peer_addr))
    }

    /// create a new connection
    pub fn create_connection(
        &mut self,
        conn_id: VsockConnId,
        local_addr: VsockAddr,
        peer_addr: Option<VsockAddr>,
        state: ConnectionState,
        is_bridge: bool,
    ) -> KResult<Arc<Mutex<Connection>>> {
        if self.connections.contains_key(&conn_id) {
            k_bail!(AddrInUse, "connection already exists");
        }
        let conn = Connection::new(conn_id, local_addr, peer_addr, state, is_bridge);
        let conn = Arc::new(Mutex::new(conn));
        start_vsock_polling();
        self.connections.insert(conn_id, conn.clone());
        debug!(
            "Created connection {:?}: local={:?}, peer={:?}",
            conn_id, local_addr, peer_addr
        );
        Ok(conn)
    }

    /// get a connection by id
    pub fn get_connection(&self, conn_id: VsockConnId) -> Option<Arc<Mutex<Connection>>> {
        self.connections.get(&conn_id).cloned()
    }

    /// remove a connection
    pub fn remove_connection(&mut self, conn_id: VsockConnId) {
        if let Some(_conn) = self.connections.remove(&conn_id) {
            stop_vsock_polling();
            debug!("Removed connection {:?}", conn_id);
        }
    }

    #[cfg(feature = "vsock_tipc_bridge")]
    fn push_bridge_event(&mut self, event: VsockBridgeEvent) {
        self.bridge_events.push_back(event);
        BRIDGE_EVENT_WAKERS.wake();
    }

    /// Pop one pending bridge event, registering `context` if no event is ready.
    #[cfg(feature = "vsock_tipc_bridge")]
    pub fn pop_bridge_event(
        &mut self,
        context: &mut PollContext<'_>,
    ) -> Result<Option<VsockBridgeEvent>, PollRegisterError> {
        if let Some(event) = self.bridge_events.pop_front() {
            return Ok(Some(event));
        }
        context.register(&BRIDGE_EVENT_WAKERS)?;
        Ok(self.bridge_events.pop_front())
    }

    fn handle_raw_event(&mut self, event: VsockTransportEvent, body: &[u8]) -> DriverResult<()> {
        let conn_id = VsockConnId {
            peer_addr: event.source,
            local_port: event.destination.port,
        };

        match event.kind {
            VsockTransportEventKind::ConnectionRequest => {
                let local_port = event.destination.port;
                #[cfg(feature = "vsock_tipc_bridge")]
                if self.bridge_ports.contains(&local_port) {
                    let info = self.accept_info(conn_id, &event);
                    if self.connections.contains_key(&conn_id) {
                        // Retransmitted REQUEST: just re-send the RESPONSE.
                        if let Some(transport) = self.raw_transport.as_ref() {
                            transport.accept(&info)?;
                        }
                        return Ok(());
                    }
                    if let Some(transport) = self.raw_transport.as_ref() {
                        transport.accept(&info)?;
                    }
                    let conn = match self.create_connection(
                        conn_id,
                        event.destination,
                        Some(event.source),
                        ConnectionState::Connected,
                        true,
                    ) {
                        Ok(conn) => conn,
                        Err(e) => {
                            error!(
                                "create_connection failed after RESPONSE sent for {conn_id:?}: {e}"
                            );
                            return Ok(());
                        }
                    };
                    {
                        let mut c = conn.lock();
                        c.info.peer_buf_alloc = event.peer_buf_alloc;
                        c.info.peer_fwd_cnt = event.peer_fwd_cnt;
                    }
                    self.push_bridge_event(VsockBridgeEvent::ConnectionRequest(conn_id));
                    return Ok(());
                }
                if let Some(queue) = self.listen_queues.get(&local_port).cloned() {
                    let info = self.accept_info(conn_id, &event);
                    if self.connections.contains_key(&conn_id) {
                        // Retransmitted REQUEST: just re-send the RESPONSE.
                        if let Some(transport) = self.raw_transport.as_ref() {
                            transport.accept(&info)?;
                        }
                        return Ok(());
                    }
                    if let Some(transport) = self.raw_transport.as_ref() {
                        transport.accept(&info)?;
                    }
                    let conn = match self.create_connection(
                        conn_id,
                        event.destination,
                        Some(event.source),
                        ConnectionState::Connected,
                        false,
                    ) {
                        Ok(conn) => conn,
                        Err(e) => {
                            error!(
                                "create_connection failed after RESPONSE sent for {conn_id:?}: {e}"
                            );
                            return Ok(());
                        }
                    };
                    {
                        let mut c = conn.lock();
                        c.info.peer_buf_alloc = event.peer_buf_alloc;
                        c.info.peer_fwd_cnt = event.peer_fwd_cnt;
                    }
                    let mut q = queue.lock();
                    if let Err(e) = q.accept_queue.push(conn_id) {
                        drop(q);
                        self.remove_connection(conn_id);
                        error!("push into accept queue failed for port {local_port}: {e}");
                        let info = self.reject_info(conn_id, &event);
                        if let Some(transport) = self.raw_transport.as_ref() {
                            transport.force_close(&info)?;
                        }
                        return Ok(());
                    }
                    q.wake();
                    return Ok(());
                }
                // Not listening on this port — reject the connection.
                let info = self.reject_info(conn_id, &event);
                if let Some(transport) = self.raw_transport.as_ref() {
                    transport.force_close(&info)?;
                }
                Ok(())
            }
            VsockTransportEventKind::Connected => {
                if let Some(conn) = self.connections.get(&conn_id) {
                    let mut c = conn.lock();
                    if c.state == ConnectionState::Connecting {
                        c.state = ConnectionState::Connected;
                        c.peer_addr = Some(event.source);
                        c.wake_connect();
                    }
                    // The peer advertises its receive buffer credit in the
                    // Connected event; record it so the first send can proceed
                    // without waiting for a separate CreditUpdate.
                    c.info.peer_buf_alloc = event.peer_buf_alloc;
                    c.info.peer_fwd_cnt = event.peer_fwd_cnt;
                    c.tx_wait_queue().notify_all(false);
                    #[cfg(feature = "vsock_tipc_bridge")]
                    {
                        let is_bridge = c.is_bridge;
                        drop(c);
                        if is_bridge {
                            self.push_bridge_event(VsockBridgeEvent::Connected(conn_id));
                        }
                    }
                    #[cfg(not(feature = "vsock_tipc_bridge"))]
                    drop(c);
                }
                Ok(())
            }
            VsockTransportEventKind::Received { length } => {
                let data = body.get(..length).unwrap_or(body);
                if let Some(conn) = self.connections.get(&conn_id) {
                    let mut c = conn.lock();
                    c.info.peer_buf_alloc = event.peer_buf_alloc;
                    c.info.peer_fwd_cnt = event.peer_fwd_cnt;
                    let written = c.push_rx_data(data);
                    if written > 0 {
                        c.wake_rx();
                    }
                    #[cfg(feature = "vsock_tipc_bridge")]
                    {
                        let is_bridge = c.is_bridge;
                        drop(c);
                        if is_bridge && written > 0 {
                            self.push_bridge_event(VsockBridgeEvent::Received(conn_id, written));
                        }
                    }
                    #[cfg(not(feature = "vsock_tipc_bridge"))]
                    drop(c);
                }
                Ok(())
            }
            VsockTransportEventKind::Disconnected => {
                self.on_disconnected(conn_id);
                #[cfg(feature = "vsock_tipc_bridge")]
                if self.is_bridge_conn(conn_id) {
                    self.push_bridge_event(VsockBridgeEvent::Disconnected(conn_id));
                }
                Ok(())
            }
            VsockTransportEventKind::CreditUpdate {
                buffer_allocation,
                forward_count,
            } => {
                if let Some(conn) = self.connections.get(&conn_id) {
                    let mut c = conn.lock();
                    c.info.peer_buf_alloc = buffer_allocation;
                    c.info.peer_fwd_cnt = forward_count;
                    c.tx_wait_queue().notify_all(false);
                    #[cfg(feature = "vsock_tipc_bridge")]
                    {
                        let is_bridge = c.is_bridge;
                        drop(c);
                        if is_bridge {
                            self.push_bridge_event(VsockBridgeEvent::CreditUpdate(conn_id));
                        }
                    }
                    #[cfg(not(feature = "vsock_tipc_bridge"))]
                    drop(c);
                }
                Ok(())
            }
            VsockTransportEventKind::CreditRequest => {
                if let Some(conn) = self.connections.get(&conn_id) {
                    let info = conn.lock().info;
                    if let Some(transport) = self.raw_transport.as_ref() {
                        transport.credit_update(&info)?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Build a [`VsockConnectionInfo`] for accepting a connection on `conn_id`
    /// using peer credit from `event`.
    fn accept_info(
        &self,
        conn_id: VsockConnId,
        event: &VsockTransportEvent,
    ) -> VsockConnectionInfo {
        VsockConnectionInfo {
            conn_id,
            buf_alloc: VSOCK_RX_BUFFER_SIZE as u32,
            fwd_cnt: 0,
            peer_buf_alloc: event.peer_buf_alloc,
            peer_fwd_cnt: event.peer_fwd_cnt,
            tx_cnt: 0,
        }
    }

    /// Build a [`VsockConnectionInfo`] for rejecting a connection request on
    /// `conn_id`. Uses our RX buffer size for `buf_alloc` and peer credit from
    /// the event; `fwd_cnt` and `tx_cnt` are 0 because we never accepted this
    /// connection.
    fn reject_info(
        &self,
        conn_id: VsockConnId,
        event: &VsockTransportEvent,
    ) -> VsockConnectionInfo {
        VsockConnectionInfo {
            conn_id,
            buf_alloc: VSOCK_RX_BUFFER_SIZE as u32,
            fwd_cnt: 0,
            peer_buf_alloc: event.peer_buf_alloc,
            peer_fwd_cnt: event.peer_fwd_cnt,
            tx_cnt: 0,
        }
    }

    /// Update connection state on a peer disconnect/RST.
    fn on_disconnected(&mut self, conn_id: VsockConnId) {
        if let Some(conn) = self.connections.get(&conn_id) {
            let mut conn_guard = conn.lock();
            conn_guard.state = ConnectionState::Closed;
            conn_guard.rx_closed = true;
            conn_guard.tx_closed = true;
            conn_guard.wake_rx();
            conn_guard.wake_connect();
            conn_guard.tx_wait_queue().notify_all(false);
            trace!("Connection {:?} disconnected", conn_id);
        }
    }

    /// Snapshot the connection info for `conn_id`. Returns `None` if the
    /// connection is not known to the manager.
    pub fn connection_info_snapshot(&self, conn_id: VsockConnId) -> Option<VsockConnectionInfo> {
        self.connections.get(&conn_id).map(|c| c.lock().info)
    }

    /// Add `len` bytes to the connection's transmitted-byte counter.
    #[cfg(feature = "vsock_tipc_bridge")]
    pub fn add_tx_bytes(&self, conn_id: VsockConnId, len: usize) {
        if let Some(conn) = self.connections.get(&conn_id) {
            conn.lock().add_tx_bytes(len);
        }
    }
}

#[cfg(feature = "vsock_tipc_bridge")]
static BRIDGE_EVENT_WAKERS: Lazy<PollSet> = Lazy::new(PollSet::new);

static_lock! {
    pub static VSOCK_CONN_MANAGER: Mutex<VsockConnectionManager> =
        Mutex::new(VsockConnectionManager::new());
}

/// Map a transport-level [`DriverError`] to a [`KError`] for socket callers.
fn map_dev_err(e: DriverError) -> KError {
    match e {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        _ => KError::BadState,
    }
}

// ------------------------------------------------------------------
// Device registration
// ------------------------------------------------------------------

static_lock! {
    static VSOCK_DEV: Mutex<Option<ClassDevice<VsockDeviceImpl>>> = Mutex::new(None);
}

/// Registers a vsock device. Only one vsock device can be registered.
///
/// The incoming `raw` handle is a low-level [`VsockDevice`] packet transport.
/// We take it over, install it into the connection manager, and start the poll
/// task. The rest of the system (AF_VSOCK socket and vsock-TIPC bridge) uses
/// [`VSOCK_CONN_MANAGER`] rather than the raw device directly.
pub fn register_vsock_dev(raw: ClassDevice<VsockDeviceImpl>) -> KResult<()> {
    let mut guard = VSOCK_DEV.lock();
    if guard.is_some() {
        k_bail!(AlreadyExists, "vsock device already registered");
    }

    let wrapper = Arc::new(RawTransportWrapper::new(raw.clone()));
    let guest_cid = VSOCK_CONN_MANAGER
        .lock()
        .set_raw_transport(wrapper as Arc<dyn VsockDevice + Send + Sync>);
    log::info!(
        "vsock manager installed raw transport (guest_cid={}), driver={}",
        guest_cid,
        raw.driver_name()
    );

    *guard = Some(raw);
    drop(guard);

    // Start polling once now so early bridge/AF_VSOCK setup can receive events.
    start_vsock_polling();

    Ok(())
}

pub fn unregister_vsock_dev(id: DeviceId) -> bool {
    let mut guard = VSOCK_DEV.lock();
    if guard.as_ref().is_none_or(|dev| dev.id() != id) {
        return false;
    }
    *guard = None;
    drop(guard);

    VSOCK_CONN_MANAGER.lock().detach_raw_transport();

    let mut state = POLLER_STATE.lock();
    state.ref_count = 0;
    true
}

// ------------------------------------------------------------------
// Polling
// ------------------------------------------------------------------

static_lock! {
    static POLLER_STATE: Mutex<PollerState> = Mutex::new(PollerState::new());
}

static POLL_BACKOFF: PollBackoff = PollBackoff::new();

struct PollerState {
    ref_count: usize,
    active: bool,
}

impl PollerState {
    const fn new() -> Self {
        Self {
            ref_count: 0,
            active: false,
        }
    }
}

struct PollBackoff {
    consecutive_idle: AtomicU64,
}

impl PollBackoff {
    const fn new() -> Self {
        Self {
            consecutive_idle: AtomicU64::new(0),
        }
    }

    fn next_interval(&self) -> TimeSpan {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval_us = match idle {
            0..=3 => 100,     //  3 ：100μs
            4..=10 => 500,    // 4-10 ：500μs
            11..=20 => 2_000, // 11-20 ：2ms
            _ => 10_000,      // 20+ ：10ms
        };
        TimeSpan::from_micros(interval_us)
    }

    fn on_activity(&self) {
        self.consecutive_idle.store(0, Ordering::Release);
    }

    fn on_idle_tick(&self) {
        self.consecutive_idle.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, u64) {
        let idle = self.consecutive_idle.load(Ordering::Relaxed);
        let interval = self.next_interval().as_micros() as u64;
        (idle, interval)
    }
}

/// Start the background vsock polling task if needed.
pub fn start_vsock_polling() {
    let mut state = POLLER_STATE.lock();
    state.ref_count += 1;
    debug!("start_vsock_polling: ref_count -> {}", state.ref_count);
    if state.ref_count == 1 {
        if !state.active {
            state.active = true;
            drop(state);
            debug!("Starting vsock poll task");
            ktask::spawn_with_name(vsock_poll_task, "vsock-poll".to_string());
        } else {
            debug!("Poll task already running");
        }
    }
}

pub fn stop_vsock_polling() {
    let mut state = POLLER_STATE.lock();
    if state.ref_count == 0 {
        // this should not happen, log a warning
        warn!("stop_vsock_polling called but ref_count already 0");
        return;
    }
    state.ref_count -= 1;
    debug!("stop_vsock_polling: ref_count -> {}", state.ref_count);
}

fn vsock_poll_task() {
    loop {
        if should_stop_vsock_poll_task() {
            break;
        }

        let _ = block_on(interruptible(poll_vsock_adaptive()));
    }
}

fn should_stop_vsock_poll_task() -> bool {
    let mut state = POLLER_STATE.lock();
    if state.ref_count != 0 {
        return false;
    }
    state.active = false;
    debug!("Vsock poll task exiting (no active connections)");
    true
}

async fn poll_vsock_adaptive() -> KResult<()> {
    let has_events = poll_vsock_devices()?;

    if has_events {
        POLL_BACKOFF.on_activity();
        ktask::yield_now();
        return Ok(());
    }

    POLL_BACKOFF.on_idle_tick();
    let interval = POLL_BACKOFF.next_interval();

    let (idle_count, interval_us) = POLL_BACKOFF.snapshot();
    if idle_count > 0 && idle_count % 10 == 0 {
        trace!("Poll frequency: idle_count={idle_count}, interval={interval_us}μs",);
    }
    ktask::future::sleep(interval).await;
    Ok(())
}

fn poll_vsock_devices() -> KResult<bool> {
    // Clone the transport handle without holding the manager lock. The actual
    // virtqueue poll may sleep/spin waiting for the device; if we held the
    // manager lock here, a concurrent sender waiting on a tx completion would
    // deadlock because the completion can only be processed while the manager
    // lock is free.
    let transport = VSOCK_CONN_MANAGER.lock().clone_raw_transport();
    let Some(transport) = transport else {
        return Ok(false);
    };

    // Drain all pending events from the RX virtqueue in one go, mirroring
    // the loop in the old two-layer architecture. This avoids per-event
    // overhead of the poll-task loop, block_on wrapper, and adaptive
    // backoff state machine.
    let mut had_event = false;
    loop {
        match transport.poll_event(&mut |event, body| {
            had_event = true;
            // Handle each event under the manager lock. The handler directly
            // calls back into the transport (accept/force_close/credit_update)
            // which is safe because poll_event releases its inner lock before
            // invoking the callback.
            VSOCK_CONN_MANAGER.lock().handle_raw_event(event, body)?;
            Ok(())
        }) {
            Ok(true) => continue, // more events pending
            Ok(false) => break,   // queue drained
            Err(e) => {
                // Log but don't abort the poll task on a single-event error.
                error!("vsock poll_event error: {:?}", e);
                break;
            }
        }
    }
    Ok(had_event)
}

// ------------------------------------------------------------------
// Socket-facing helpers
// ------------------------------------------------------------------

pub fn vsock_connect(conn_id: VsockConnId) -> KResult<()> {
    let (transport, info) = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let info = manager
            .connection_info_snapshot(conn_id)
            .unwrap_or_default();
        let transport = manager
            .clone_raw_transport()
            .ok_or(DriverError::BadState)
            .map_err(map_dev_err)?;
        (transport, info)
    };
    transport.connect(&info).map_err(|e| {
        VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
        map_dev_err(e)
    })
}

/// Send `buf` on `conn_id`, blocking on the connection's TX wait queue when
/// the peer has no credit.
///
/// The raw transport is all-or-nothing: it either queues the whole packet or
/// returns [`DriverError::WouldBlock`], so there is no point in retrying with
/// a smaller chunk. For blocking sockets the caller waits on the TX wait queue
/// until the peer advertises enough credit or the connection is closed;
/// for non-blocking sockets [`KError::WouldBlock`] is returned immediately.
pub fn vsock_send(
    conn_id: VsockConnId,
    buf: &[u8],
    nonblocking: bool,
    tx_wait_queue: &WaitQueue,
) -> KResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }

    loop {
        let (transport, info) = {
            let manager = VSOCK_CONN_MANAGER.lock();
            let info = manager
                .connection_info_snapshot(conn_id)
                .unwrap_or_default();
            let transport = manager
                .clone_raw_transport()
                .ok_or(DriverError::BadState)
                .map_err(map_dev_err)?;
            (transport, info)
        };
        match transport.send(&info, buf) {
            Ok(len) => {
                return Ok(len);
            }
            Err(DriverError::WouldBlock) if nonblocking => {
                return Err(map_dev_err(DriverError::WouldBlock));
            }
            Err(DriverError::WouldBlock) => {
                // Use wait_until to close the lost-wakeup window.
                // Connection absent ⇒ closed, return true so the waiter
                // wakes up and falls into the ConnectionReset path below.
                let check_conn_id = conn_id;
                tx_wait_queue.wait_until(|| {
                    let manager = VSOCK_CONN_MANAGER.lock();
                    match manager.get_connection(check_conn_id) {
                        None => true, // connection removed → treat as closed
                        Some(c) => {
                            let c = c.lock();
                            c.state() != ConnectionState::Connected
                                || c.tx_closed()
                                || c.info.peer_free() as usize >= buf.len()
                        }
                    }
                });
                // Distinguish: credit available vs. connection closed.
                let manager = VSOCK_CONN_MANAGER.lock();
                let alive = manager.get_connection(check_conn_id).is_some_and(|c| {
                    let c = c.lock();
                    c.state() == ConnectionState::Connected && !c.tx_closed()
                });
                if !alive {
                    return Err(KError::ConnectionReset);
                }
            }
            Err(e) => return Err(map_dev_err(e)),
        }
    }
}

pub fn vsock_disconnect(conn_id: VsockConnId) -> KResult<()> {
    let (transport, info) = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let info = manager
            .connection_info_snapshot(conn_id)
            .unwrap_or_default();
        let transport = manager
            .clone_raw_transport()
            .ok_or(DriverError::BadState)
            .map_err(map_dev_err)?;
        (transport, info)
    };
    transport.shutdown(&info).map_err(map_dev_err)
}

// ------------------------------------------------------------------
// Bridge-facing helpers
// ------------------------------------------------------------------

/// Create a reverse (TA-to-host) bridge connection and send a connect request.
///
/// `pre_connect` is called after the connection is registered in the manager
/// but before the connect packet is sent to the peer. This gives the bridge
/// a chance to insert its `BridgeConnection` so event handlers can find it
/// before the peer responds.
///
/// Returns the connection id on success.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn create_bridge_connection(
    target_addr: VsockAddr,
    pre_connect: impl FnOnce(VsockConnId),
) -> KResult<VsockConnId> {
    let (conn_id, info, transport) = {
        let mut manager = VSOCK_CONN_MANAGER.lock();
        let local_cid = manager.guest_cid();
        if local_cid == 0 {
            k_bail!(BadState, "vsock transport not installed");
        }
        let local_port = manager.allocate_port()?;
        let local_addr = VsockAddr {
            cid: local_cid,
            port: local_port,
        };
        let conn_id = VsockConnId {
            peer_addr: target_addr,
            local_port,
        };
        manager.create_connection(
            conn_id,
            local_addr,
            Some(target_addr),
            ConnectionState::Connecting,
            true,
        )?;
        let info = manager
            .connection_info_snapshot(conn_id)
            .unwrap_or_default();
        let transport = manager
            .clone_raw_transport()
            .ok_or(DriverError::BadState)
            .map_err(map_dev_err)?;
        (conn_id, info, transport)
    };

    // Invoke the pre-connect hook so the bridge can register its connection
    // before the connect packet reaches the peer.
    pre_connect(conn_id);

    transport.connect(&info).map_err(|e| {
        VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
        map_dev_err(e)
    })?;
    Ok(conn_id)
}

/// Send a complete bridge record. Returns the total bytes accepted once the
/// whole buffer has been sent, or an error if the transport cannot accept
/// more data.
///
/// The underlying transport is all-or-nothing (see [`VsockDevice::send`]), so
/// a single call either queues the entire buffer or returns
/// [`DriverError::WouldBlock`].  The loop only retries after a credit-update
/// wake-up when the peer's buffer was full.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn send_bridge(conn_id: VsockConnId, buf: &[u8]) -> DriverResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }

    loop {
        let (transport, info, tx_wait_queue) = {
            let manager = VSOCK_CONN_MANAGER.lock();
            let conn = manager
                .get_connection(conn_id)
                .ok_or(DriverError::InvalidInput)?;
            let (info, tx_wait_queue) = {
                let c = conn.lock();
                (c.info, c.tx_wait_queue())
            };
            let transport = manager.clone_raw_transport().ok_or(DriverError::BadState)?;
            (transport, info, tx_wait_queue)
        };
        match transport.send(&info, buf) {
            Ok(sent) => {
                VSOCK_CONN_MANAGER.lock().add_tx_bytes(conn_id, sent);
                return Ok(sent);
            }
            Err(DriverError::WouldBlock) => {
                // Use wait_until to close the lost-wakeup window.
                // Connection absent ⇒ closed, return true so the waiter
                // wakes up and falls into the error path below.
                let check_conn_id = conn_id;
                tx_wait_queue.wait_until(|| {
                    let manager = VSOCK_CONN_MANAGER.lock();
                    match manager.get_connection(check_conn_id) {
                        None => true, // connection removed → treat as closed
                        Some(c) => {
                            let c = c.lock();
                            c.state() != ConnectionState::Connected
                                || c.tx_closed()
                                || c.info.peer_free() as usize >= buf.len()
                        }
                    }
                });
                let manager = VSOCK_CONN_MANAGER.lock();
                let alive = manager.get_connection(check_conn_id).is_some_and(|c| {
                    let c = c.lock();
                    c.state() == ConnectionState::Connected && !c.tx_closed()
                });
                if !alive {
                    return Err(DriverError::Io);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Receive up to `buf.len()` bytes from a bridge connection's receive buffer.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn recv_bridge(conn_id: VsockConnId, buf: &mut [u8]) -> DriverResult<usize> {
    let count = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let Some(conn) = manager.connections.get(&conn_id) else {
            return Err(DriverError::InvalidInput);
        };
        let mut c = conn.lock();
        if c.rx_buffer_used() == 0 {
            0
        } else {
            let (left, right) = c.rx_slices();
            let mut count = 0;
            if !buf.is_empty() {
                let n = buf.len().min(left.len());
                buf[..n].copy_from_slice(&left[..n]);
                count = n;
                if count < buf.len() && !right.is_empty() {
                    let n2 = (buf.len() - count).min(right.len());
                    buf[count..count + n2].copy_from_slice(&right[..n2]);
                    count += n2;
                }
            }
            c.advance_rx_read(count);
            count
        }
    };
    if count > 0 {
        let _ = advance_rx_credit(conn_id, count)
            .map_err(|e| warn!("recv_bridge: failed to update credit: {e:?}"));
    }
    Ok(count)
}

/// Disconnect a bridge connection and remove it from the manager.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn disconnect_bridge(conn_id: VsockConnId) -> DriverResult<()> {
    let (transport, info, tx_wq) = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let already_closed = manager
            .get_connection(conn_id)
            .is_some_and(|c| c.lock().state() == ConnectionState::Closed);
        let tx_wq = manager
            .get_connection(conn_id)
            .map(|c| c.lock().tx_wait_queue().clone());
        if already_closed {
            (None, VsockConnectionInfo::default(), tx_wq)
        } else {
            let info = manager
                .connection_info_snapshot(conn_id)
                .unwrap_or_default();
            let transport = manager.clone_raw_transport();
            (transport, info, tx_wq)
        }
    };
    if let Some(transport) = transport
        && let Err(e) = transport.shutdown(&info)
    {
        error!("shutdown packet failed for {:?}: {e}", conn_id);
    }
    // Wake any sender blocked on the TX wait queue before removing the
    // connection, so that wait_until sees the connection is gone and returns.
    if let Some(wq) = tx_wq {
        wq.notify_all(false);
    }
    VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
    Ok(())
}

/// Forcibly abort a bridge connection and remove it from the manager.
#[cfg(feature = "vsock_tipc_bridge")]
pub fn abort_bridge(conn_id: VsockConnId) -> DriverResult<()> {
    let (transport, info, tx_wq) = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let already_closed = manager
            .get_connection(conn_id)
            .is_some_and(|c| c.lock().state() == ConnectionState::Closed);
        let tx_wq = manager
            .get_connection(conn_id)
            .map(|c| c.lock().tx_wait_queue().clone());
        if already_closed {
            (None, VsockConnectionInfo::default(), tx_wq)
        } else {
            let info = manager
                .connection_info_snapshot(conn_id)
                .unwrap_or_default();
            let transport = manager.clone_raw_transport();
            (transport, info, tx_wq)
        }
    };
    if let Some(transport) = transport
        && let Err(e) = transport.force_close(&info)
    {
        error!("force_close packet failed for {:?}: {e}", conn_id);
    }
    // Wake any sender blocked on the TX wait queue before removing the
    // connection, so that wait_until sees the connection is gone and returns.
    if let Some(wq) = tx_wq {
        wq.notify_all(false);
    }
    VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
    Ok(())
}

/// Update the forward count after the application has consumed `count` bytes
/// and send a credit update to the peer when needed.
///
/// Mirrors Linux's `virtio_transport_stream_do_dequeue()`: the peer accounts
/// against the last `fwd_cnt` we reported, so only push an update once the
/// free space visible to the peer drops below a low watermark. While the
/// peer still has plenty of credit, an explicit update is pure overhead.
pub fn advance_rx_credit(conn_id: VsockConnId, count: usize) -> DriverResult<()> {
    let (transport, info, conn) = {
        let manager = VSOCK_CONN_MANAGER.lock();
        let conn = manager
            .get_connection(conn_id)
            .ok_or(DriverError::InvalidInput)?;
        let mut c = conn.lock();
        c.info.fwd_cnt = c.info.fwd_cnt.wrapping_add(count as u32);
        let unreported = c.info.fwd_cnt.wrapping_sub(c.last_sent_fwd_cnt);
        let peer_visible_free = c.info.buf_alloc.saturating_sub(unreported);
        if unreported == 0 || peer_visible_free >= c.info.buf_alloc / 4 {
            return Ok(());
        }
        let info = c.info;
        drop(c);
        let transport = manager.clone_raw_transport().ok_or(DriverError::BadState)?;
        (transport, info, conn)
    };
    transport.credit_update(&info)?;
    conn.lock().last_sent_fwd_cnt = info.fwd_cnt;
    Ok(())
}

/// Wrapper that turns a kclass `ClassDevice<VsockDeviceImpl>` into a
/// [`VsockDevice`] by calling `ClassDevice::with()` on each method.
///
/// The connection manager owns this wrapper and uses it to drive the raw
/// virtqueue without knowing about kclass locking.
struct RawTransportWrapper(ClassDevice<VsockDeviceImpl>);

impl RawTransportWrapper {
    fn new(device: ClassDevice<VsockDeviceImpl>) -> Self {
        Self(device)
    }

    fn with_device<R>(&self, f: impl FnOnce(&dyn VsockDevice) -> R) -> R {
        self.0.with(|device| f(&**device))
    }
}

impl Device for RawTransportWrapper {
    fn name(&self) -> &str {
        "vsock-raw-transport-wrapper"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Vsock
    }

    fn irq(&self) -> Option<usize> {
        self.0.irq()
    }
}

impl VsockDevice for RawTransportWrapper {
    fn guest_cid(&self) -> u64 {
        self.with_device(|t| t.guest_cid())
    }

    fn listen(&self, port: u32) {
        self.with_device(|t| t.listen(port));
    }

    fn unlisten(&self, port: u32) {
        self.with_device(|t| t.unlisten(port));
    }

    fn connect(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_device(|t| t.connect(info))
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_device(|t| t.accept(info))
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_device(|t| t.force_close(info))
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize> {
        self.with_device(|t| t.send(info, buf))
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_device(|t| t.shutdown(info))
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with_device(|t| t.credit_update(info))
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        self.with_device(|t| t.poll_event(handler))
    }
}
