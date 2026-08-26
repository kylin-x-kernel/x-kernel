// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trusty-compatible vsock-TIPC bridge.
//!
//! The bridge is a consumer of the vsock connection manager. It registers the
//! well-known bridge ports (0-4) with the manager, receives per-connection events
//! through the manager's bridge event queue, and reads/writes record data through
//! the manager's connection API. The raw device is never touched directly.

use alloc::{
    collections::VecDeque,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    cell::Cell,
    future::poll_fn,
    str,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Poll,
};

use kclass::prelude::DriverError;
use kerrno::{KError, KResult};
use klazy::Lazy;
use kpoll::{PollRegisterError, PollRegistrations, PollSet};
use ksync::Mutex;
use ktask::future::block_on;
use ktime_types::TimeSpan;
use tipc::{
    Handle, HandleEventMask, HandleSet, HandleSetCommand, HandleSetEntry, IPC_CHAN_MAX_BUF_SIZE,
    IPC_PORT_PATH_MAX, IpcChan, IpcConnectFlags, IpcMsgInfo, IpcPort, IpcPortFlags, IpcUuid,
    UEvent, ipc_port_connect_async, ipc_port_create, ipc_port_publish,
};

use super::{
    VsockConnId,
    bridge_connection::{BridgeConnection, BridgeConnectionState},
    bridge_port_map::{BRIDGE_PORT_MAP, TIPC_TO_VSOCK_MAP, TipcToVsockMapping, bridge_mapping},
    connection_manager::{
        VSOCK_CONN_MANAGER, VsockBridgeEvent, abort_bridge, create_bridge_connection,
        disconnect_bridge, recv_bridge, send_bridge,
    },
};

const BRIDGE_RECV_BUFS: usize = 4;
const PORT_COOKIE_BASE: usize = 1usize << (usize::BITS - 1);
/// Dynamic bridge (port 0) status byte returned to host after service-name handshake.
const DYNAMIC_CONNECT_STATUS_OK: u8 = 0;
/// Definitive connect failure (invalid name, TIPC error). Not sent while waiting for publish.
const DYNAMIC_CONNECT_STATUS_REJECT: u8 = 1;
const CHANNEL_EVENTS: HandleEventMask = HandleEventMask::READY
    .union(HandleEventMask::MSG)
    .union(HandleEventMask::SEND_UNBLOCKED)
    .union(HandleEventMask::HUP)
    .union(HandleEventMask::ERROR);
const PORT_EVENTS: HandleEventMask = HandleEventMask::READY
    .union(HandleEventMask::HUP)
    .union(HandleEventMask::ERROR);

static VSOCK_BRIDGE: Lazy<VsockBridge> = Lazy::new(VsockBridge::new);
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

struct PublishedPort {
    port: Arc<IpcPort>,
    mapping_index: usize,
}

/// Initializes the bridge against the registered vsock connection manager.
pub fn init() -> KResult {
    VSOCK_BRIDGE.init()
}

/// Starts bridge worker tasks once the scheduler can spawn on every CPU.
pub fn start() {
    VSOCK_BRIDGE.start()
}

struct VsockBridge {
    connections: Mutex<Vec<BridgeConnection>>,
    rx_queue: Mutex<VecDeque<VsockBridgeEvent>>,
    rx_waiters: PollSet,
    handle_set: Arc<HandleSet>,
    ports: Mutex<Vec<PublishedPort>>,
    start_requested: AtomicBool,
    runtime_started: AtomicBool,
}

impl VsockBridge {
    fn new() -> Self {
        Self {
            connections: Mutex::new(Vec::new()),
            rx_queue: Mutex::new(VecDeque::new()),
            rx_waiters: PollSet::new(),
            handle_set: HandleSet::handle_set_create(),
            ports: Mutex::new(Vec::new()),
            start_requested: AtomicBool::new(false),
            runtime_started: AtomicBool::new(false),
        }
    }

    fn init(&self) -> KResult {
        {
            let mut manager = VSOCK_CONN_MANAGER.lock();
            for mapping in BRIDGE_PORT_MAP {
                manager.listen_bridge_port(mapping.port);
            }
        }

        self.publish_reverse_ports()?;
        if self.start_requested.load(Ordering::Acquire) {
            self.start_runtime_once();
        }
        Ok(())
    }

    fn start(&self) {
        self.start_requested.store(true, Ordering::Release);
        self.start_runtime_once();
    }

    fn publish_reverse_ports(&self) -> KResult {
        let mut ports = self.ports.lock();
        if !ports.is_empty() {
            return Ok(());
        }

        for (index, mapping) in TIPC_TO_VSOCK_MAP.iter().enumerate() {
            let port = ipc_port_create(
                IpcUuid::default(),
                mapping.tipc_service.to_string(),
                BRIDGE_RECV_BUFS,
                IPC_CHAN_MAX_BUF_SIZE,
                IpcPortFlags::ALLOW_TA_CONNECT,
            )?;
            ipc_port_publish(&port)?;
            let handle: Arc<dyn Handle> = port.clone();
            self.handle_set.handle_set_ctrl(
                HandleSetCommand::Add,
                HandleSetEntry {
                    handle_id: port_handle_id(index),
                    handle,
                    event: PORT_EVENTS,
                    cookie: PORT_COOKIE_BASE | index,
                },
            )?;
            ports.push(PublishedPort {
                port,
                mapping_index: index,
            });
        }
        Ok(())
    }

    fn start_runtime_once(&self) {
        if self.runtime_started.swap(true, Ordering::SeqCst) {
            return;
        }
        ktask::spawn_with_name(rx_task, "vsock-tipc-rx".to_string());
        ktask::spawn_with_name(tx_task, "vsock-tipc-tx".to_string());
        crate::vsock::connection_manager::start_vsock_polling();
    }

    fn has_connection(&self, conn_id: VsockConnId) -> bool {
        self.connections
            .lock()
            .iter()
            .any(|conn| conn.conn_id() == conn_id)
    }

    fn push_rx_event(&self, event: VsockBridgeEvent) {
        self.rx_queue.lock().push_back(event);
        self.rx_waiters.wake();
    }

    fn pop_rx_event(&self) -> Result<VsockBridgeEvent, PollRegisterError> {
        let mut registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            if let Some(event) = self.rx_queue.lock().pop_front() {
                return Poll::Ready(Ok(event));
            }

            let mut context = registrations.context(cx);
            if let Err(err) = context.register(&self.rx_waiters) {
                return Poll::Ready(Err(err));
            }
            if let Ok(Some(event)) = VSOCK_CONN_MANAGER.lock().pop_bridge_event(&mut context) {
                return Poll::Ready(Ok(event));
            }
            if let Some(event) = self.rx_queue.lock().pop_front() {
                return Poll::Ready(Ok(event));
            }

            Poll::Pending
        }))
    }

    fn wait_handle_event(&self) -> UEvent {
        let mut registrations = PollRegistrations::new();
        loop {
            match self.handle_set.poll_one() {
                Ok(Some(event)) => return event,
                Ok(None) | Err(KError::NotFound) => {
                    let wait_result = block_on(poll_fn(|cx| {
                        let mut context = registrations.context(cx);
                        if let Err(err) = self
                            .handle_set
                            .register(&mut context, HandleEventMask::READY)
                        {
                            return Poll::Ready(Err(err));
                        }
                        drop(context);
                        if self.handle_set.poll(false).contains(HandleEventMask::READY) {
                            Poll::Ready(Ok(()))
                        } else {
                            Poll::Pending
                        }
                    }));
                    if let Err(err) = wait_result {
                        warn!("vsock bridge handle-set register failed: {err:?}");
                        ktask::sleep(TimeSpan::from_millis(10));
                    }
                }
                Err(err) => {
                    warn!("vsock bridge handle-set poll failed: {err:?}");
                    ktask::sleep(TimeSpan::from_millis(10));
                }
            }
        }
    }

    fn handle_rx_event(&self, event: VsockBridgeEvent) {
        match event {
            VsockBridgeEvent::ConnectionRequest(conn_id) => self.on_connection_request(conn_id),
            VsockBridgeEvent::Received(conn_id, len) => self.on_received(conn_id, len),
            VsockBridgeEvent::Connected(conn_id) => self.on_connected(conn_id),
            VsockBridgeEvent::Disconnected(conn_id) => self.close_connection(conn_id),
            VsockBridgeEvent::CreditUpdate(conn_id) => {
                if self.has_connection(conn_id) {
                    trace!("vsock bridge credit update for {conn_id:?}");
                } else {
                    trace!("vsock bridge credit update for unknown connection {conn_id:?}");
                }
            }
        }
    }

    fn on_connection_request(&self, conn_id: VsockConnId) {
        let Some(mapping) = bridge_mapping(conn_id.local_port) else {
            return;
        };
        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut conn = BridgeConnection::new(token, conn_id.peer_addr, conn_id.local_port);
        if !mapping.tipc_service.is_empty()
            && let Err(err) = self.connect_tipc(
                &mut conn,
                mapping.tipc_service,
                IpcConnectFlags::WAIT_FOR_PORT | IpcConnectFlags::ASYNC,
            )
        {
            warn!(
                "vsock bridge failed to connect fixed port {} to {}: {err:?}",
                mapping.port, mapping.tipc_service
            );
            let _ = abort_bridge(conn_id);
            return;
        }
        self.connections.lock().push(conn);
    }

    fn on_received(&self, conn_id: VsockConnId, len: usize) {
        let state = self
            .connections
            .lock()
            .iter()
            .find(|conn| conn.conn_id() == conn_id)
            .map(|conn| conn.state());
        match state {
            Some(BridgeConnectionState::VsockOnly) => self.on_dynamic_service_name(conn_id, len),
            Some(BridgeConnectionState::Active) => self.forward_vsock_to_tipc(conn_id, len),
            Some(
                BridgeConnectionState::TipcConnecting | BridgeConnectionState::TipcSendBlocked,
            ) => {
                // TIPC is not ready to consume the packet yet. Re-queue the event
                // locally and retry after a short backoff.
                self.push_rx_event(VsockBridgeEvent::Received(conn_id, len));
                ktask::sleep(TimeSpan::from_millis(10));
            }
            Some(_) => {
                warn!("vsock bridge received data in invalid state for {conn_id:?}");
                self.close_connection(conn_id);
            }
            None => {
                warn!("vsock bridge drop Received(len={len}) with no connection: {conn_id:?}");
            }
        }
    }

    fn on_dynamic_service_name(&self, conn_id: VsockConnId, len: usize) {
        match self.recv_exact_record(conn_id, len) {
            Ok(data) => {
                let name = match parse_service_name(&data) {
                    Ok(name) => name,
                    Err(err) => {
                        warn!("vsock bridge invalid dynamic service name: {err:?}");
                        self.reject_dynamic_connect(conn_id);
                        return;
                    }
                };
                let mut conns = self.connections.lock();
                let Some(conn) = conns.iter_mut().find(|conn| conn.conn_id() == conn_id) else {
                    return;
                };
                if let Err(err) = self.connect_tipc(
                    conn,
                    &name,
                    IpcConnectFlags::WAIT_FOR_PORT | IpcConnectFlags::ASYNC,
                ) {
                    warn!("vsock bridge failed to connect dynamic service {name}: {err:?}");
                    drop(conns);
                    self.reject_dynamic_connect(conn_id);
                }
            }
            Err(err) => {
                warn!("vsock bridge failed to read dynamic service name: {err:?}");
                self.reject_dynamic_connect(conn_id);
            }
        }
    }

    fn forward_vsock_to_tipc(&self, conn_id: VsockConnId, len: usize) {
        match self.recv_exact_record(conn_id, len) {
            Ok(data) => {
                let mut conns = self.connections.lock();
                let Some(conn) = conns.iter_mut().find(|conn| conn.conn_id() == conn_id) else {
                    return;
                };
                if conn.has_pending_rx() {
                    // A previous record is still blocked on TIPC. Re-queue and
                    // wait for the channel to unblock.
                    self.push_rx_event(VsockBridgeEvent::Received(conn_id, len));
                    return;
                }
                if let Err(err) = conn.stage_rx(&data).and_then(|_| conn.tipc_try_send()) {
                    warn!("vsock bridge failed to send vsock data to TIPC: {err:?}");
                    drop(conns);
                    self.close_connection(conn_id);
                }
            }
            Err(err) => {
                warn!("vsock bridge failed to read vsock data: {err:?}");
                self.close_connection(conn_id);
            }
        }
    }

    fn connect_tipc(
        &self,
        conn: &mut BridgeConnection,
        service_name: &str,
        flags: IpcConnectFlags,
    ) -> KResult {
        let channel = ipc_port_connect_async(IpcUuid::default(), service_name, flags)?;
        conn.set_tipc_channel(service_name, channel.clone());
        conn.set_state(BridgeConnectionState::TipcConnecting);
        self.add_channel_handle(conn.token(), channel)
    }

    fn add_channel_handle(&self, token: usize, channel: Arc<IpcChan>) -> KResult {
        let handle: Arc<dyn Handle> = channel;
        self.handle_set.handle_set_ctrl(
            HandleSetCommand::Add,
            HandleSetEntry {
                handle_id: channel_handle_id(token),
                handle,
                event: CHANNEL_EVENTS,
                cookie: token,
            },
        )?;
        Ok(())
    }

    fn remove_channel_handle(&self, token: usize, channel: Arc<IpcChan>) {
        let handle: Arc<dyn Handle> = channel;
        let _ = self.handle_set.handle_set_ctrl(
            HandleSetCommand::Delete,
            HandleSetEntry {
                handle_id: channel_handle_id(token),
                handle,
                event: CHANNEL_EVENTS,
                cookie: token,
            },
        );
    }

    fn recv_exact_record(&self, conn_id: VsockConnId, len: usize) -> KResult<Vec<u8>> {
        if len > IPC_CHAN_MAX_BUF_SIZE {
            return Err(KError::OutOfRange);
        }
        let mut data = vec![0; len];
        let mut offset = 0;
        while offset < len {
            let read = recv_bridge(conn_id, &mut data[offset..]).map_err(map_dev_err)?;
            if read == 0 {
                return Err(KError::Io);
            }
            offset += read;
        }
        Ok(data)
    }

    fn on_connected(&self, conn_id: VsockConnId) {
        let channel = {
            let mut conns = self.connections.lock();
            let Some(conn) = conns.iter_mut().find(|conn| conn.conn_id() == conn_id) else {
                return;
            };
            if conn.state() != BridgeConnectionState::TipcOnly {
                return;
            }
            conn.set_state(BridgeConnectionState::Active);
            conn.channel().ok()
        };
        if let Some(channel) = channel {
            let token = self
                .connections
                .lock()
                .iter()
                .find(|conn| conn.conn_id() == conn_id)
                .map(|conn| conn.token());
            if let Some(token) = token
                && let Err(err) = self.add_channel_handle(token, channel)
            {
                warn!("vsock bridge failed to add reverse channel to handle set: {err:?}");
                self.close_connection(conn_id);
            }
        }
    }

    fn reject_dynamic_connect(&self, conn_id: VsockConnId) {
        let should_reject = self
            .connections
            .lock()
            .iter()
            .any(|conn| conn.conn_id() == conn_id && conn.local_port() == 0);
        if should_reject {
            let _ = send_bridge(conn_id, &[DYNAMIC_CONNECT_STATUS_REJECT]);
        }
        let _ = abort_bridge(conn_id);
    }

    fn close_connection(&self, conn_id: VsockConnId) {
        let removed = {
            let mut conns = self.connections.lock();
            let Some(index) = conns.iter().position(|conn| conn.conn_id() == conn_id) else {
                return;
            };
            conns.remove(index)
        };
        if let Ok(channel) = removed.channel() {
            self.remove_channel_handle(removed.token(), channel.clone());
            channel.close();
        }
        let _ = disconnect_bridge(conn_id);
    }

    fn handle_tipc_event(&self, event: UEvent) {
        if event.cookie & PORT_COOKIE_BASE != 0 {
            self.handle_port_event(event);
            return;
        }
        self.handle_channel_event(event);
    }

    fn handle_port_event(&self, event: UEvent) {
        if event
            .event
            .intersects(HandleEventMask::HUP | HandleEventMask::ERROR)
        {
            warn!("vsock bridge reverse TIPC port reported error: {event:?}");
            return;
        }
        if !event.event.contains(HandleEventMask::READY) {
            return;
        }
        let index = event.cookie & !PORT_COOKIE_BASE;
        let Some(mapping) = TIPC_TO_VSOCK_MAP.get(index) else {
            return;
        };
        loop {
            match self.accept_reverse_tipc(mapping) {
                Ok(()) => {}
                Err(KError::WouldBlock) => break,
                Err(err) => {
                    warn!(
                        "vsock bridge failed to accept reverse TIPC port {}: {err:?}",
                        mapping.tipc_service
                    );
                    break;
                }
            }
        }
    }

    fn accept_reverse_tipc(&self, mapping: &TipcToVsockMapping) -> KResult {
        let port = {
            let ports = self.ports.lock();
            ports
                .iter()
                .find(|port| {
                    TIPC_TO_VSOCK_MAP[port.mapping_index].tipc_service == mapping.tipc_service
                })
                .map(|port| port.port.clone())
                .ok_or(KError::NotFound)?
        };
        let (channel, client_uuid) = port.ipc_port_accept()?;
        if !uuid_allowed(client_uuid, mapping.allowed_uuids) {
            channel.close();
            return Err(KError::PermissionDenied);
        }

        let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);

        // Create the connection in the manager and insert the BridgeConnection
        // before sending the connect request, so event handlers can find it
        // before the peer responds.
        let pre_connect_conn_id = Cell::new(None);
        let conn_id = create_bridge_connection(mapping.target_addr, |conn_id| {
            pre_connect_conn_id.set(Some(conn_id));
            let conn = BridgeConnection::new_tipc_only(
                token,
                mapping.target_addr,
                conn_id.local_port,
                mapping.tipc_service,
                channel.clone(),
            );
            self.connections.lock().push(conn);
        })
        .inspect_err(|_err| {
            // The pre_connect hook already inserted a BridgeConnection;
            // remove it to avoid leaking the entry (ephemeral port reuse
            // would otherwise create duplicate entries for the same conn_id).
            if let Some(cid) = pre_connect_conn_id.get() {
                let mut conns = self.connections.lock();
                conns.retain(|c| c.conn_id() != cid);
            }
            channel.close();
        })?;
        if let Err(err) = self.add_channel_handle(token, channel) {
            self.close_connection(conn_id);
            return Err(err);
        }
        Ok(())
    }

    fn handle_channel_event(&self, event: UEvent) {
        let conn_id = {
            let conns = self.connections.lock();
            conns
                .iter()
                .find(|conn| conn.token() == event.cookie)
                .map(|conn| conn.conn_id())
        };
        let Some(conn_id) = conn_id else {
            return;
        };

        let should_close = event
            .event
            .intersects(HandleEventMask::HUP | HandleEventMask::ERROR);
        let has_msg = event.event.contains(HandleEventMask::MSG);
        match (should_close, has_msg) {
            (true, false) => {
                // A peer that closes before sending data gives the bridge no
                // TIPC payload to drain. Close immediately so dynamic connects
                // are rejected instead of reporting a transient READY edge.
                self.close_channel_event(conn_id);
            }
            (true, true) => {
                // A channel may report MSG and HUP together when the peer responded and
                // closed immediately. Drain the queued message before cleanup.
                self.forward_tipc_to_vsock(conn_id);
                self.close_channel_event(conn_id);
            }
            (false, _) => {
                if event.event.contains(HandleEventMask::READY) {
                    self.on_tipc_ready(conn_id);
                }
                if event.event.contains(HandleEventMask::SEND_UNBLOCKED) {
                    self.on_tipc_send_unblocked(conn_id);
                }
                if has_msg {
                    self.forward_tipc_to_vsock(conn_id);
                }
            }
        }
    }

    fn close_channel_event(&self, conn_id: VsockConnId) {
        let dynamic_connecting = self
            .connections
            .lock()
            .iter()
            .find(|conn| conn.conn_id() == conn_id)
            .is_some_and(|conn| {
                conn.local_port() == 0 && conn.state() == BridgeConnectionState::TipcConnecting
            });
        if dynamic_connecting {
            self.reject_dynamic_connect(conn_id);
        } else {
            self.close_connection(conn_id);
        }
    }

    fn on_tipc_ready(&self, conn_id: VsockConnId) {
        let send_dynamic_success = {
            let mut conns = self.connections.lock();
            let Some(conn) = conns.iter_mut().find(|conn| conn.conn_id() == conn_id) else {
                return;
            };
            if conn.state() != BridgeConnectionState::TipcConnecting {
                return;
            }
            conn.set_state(BridgeConnectionState::Active);
            conn.local_port() == 0
        };
        if send_dynamic_success {
            let result = send_bridge(conn_id, &[DYNAMIC_CONNECT_STATUS_OK]);
            if result.is_err() {
                warn!("vsock bridge failed to send dynamic success byte: {result:?}");
                self.close_connection(conn_id);
            }
        }
    }

    fn on_tipc_send_unblocked(&self, conn_id: VsockConnId) {
        let mut conns = self.connections.lock();
        let Some(conn) = conns.iter_mut().find(|conn| conn.conn_id() == conn_id) else {
            return;
        };
        if let Err(err) = conn.tipc_try_send() {
            warn!("vsock bridge failed to retry TIPC send: {err:?}");
            drop(conns);
            self.close_connection(conn_id);
        }
    }

    fn forward_tipc_to_vsock(&self, conn_id: VsockConnId) {
        loop {
            let channel = {
                let conns = self.connections.lock();
                let Some(conn) = conns.iter().find(|conn| conn.conn_id() == conn_id) else {
                    return;
                };
                match conn.channel() {
                    Ok(channel) => channel,
                    Err(_) => return,
                }
            };

            let msg = match channel.ipc_get_msg() {
                Ok(msg) => msg,
                Err(KError::WouldBlock) => return,
                Err(err) => {
                    warn!("vsock bridge failed to get TIPC message: {err:?}");
                    self.close_connection(conn_id);
                    return;
                }
            };

            let result = self.read_tipc_msg(&channel, msg).and_then(|data| {
                channel.ipc_put_msg(msg.id)?;
                send_bridge(conn_id, &data).map_err(map_dev_err)
            });

            match result {
                Ok(sent) => {
                    if let Some(conn) = self
                        .connections
                        .lock()
                        .iter_mut()
                        .find(|conn| conn.conn_id() == conn_id)
                    {
                        conn.add_tx_bytes(sent);
                    }
                }
                Err(err) => {
                    warn!("vsock bridge failed to forward TIPC message to vsock: {err:?}");
                    self.close_connection(conn_id);
                    return;
                }
            }
        }
    }

    fn read_tipc_msg(&self, channel: &Arc<IpcChan>, msg: IpcMsgInfo) -> KResult<Vec<u8>> {
        if msg.num_handles != 0 {
            return Err(KError::Unsupported);
        }
        if msg.len > IPC_CHAN_MAX_BUF_SIZE {
            return Err(KError::OutOfRange);
        }
        let mut data = vec![0; msg.len];
        let read_len = channel.ipc_read_msg(msg.id, 0, &mut data)?;
        if read_len != msg.len {
            return Err(KError::Io);
        }
        Ok(data)
    }
}

fn rx_task() {
    loop {
        match VSOCK_BRIDGE.pop_rx_event() {
            Ok(event) => VSOCK_BRIDGE.handle_rx_event(event),
            Err(err) => {
                warn!("vsock bridge receive registration failed: {err}");
                ktask::yield_now();
            }
        }
    }
}

fn tx_task() {
    loop {
        let event = VSOCK_BRIDGE.wait_handle_event();
        VSOCK_BRIDGE.handle_tipc_event(event);
    }
}

fn parse_service_name(data: &[u8]) -> KResult<String> {
    let data = data.strip_suffix(b"\n").unwrap_or(data);
    let name = str::from_utf8(data).map_err(|_| KError::InvalidInput)?;
    if name.is_empty() || name.len() >= IPC_PORT_PATH_MAX || name.as_bytes().contains(&0) {
        return Err(KError::InvalidInput);
    }
    Ok(name.to_string())
}

fn uuid_allowed(uuid: IpcUuid, allowed_uuids: &[IpcUuid]) -> bool {
    allowed_uuids.is_empty() || allowed_uuids.contains(&uuid)
}

fn channel_handle_id(token: usize) -> i32 {
    token as i32
}

fn port_handle_id(index: usize) -> i32 {
    -1 - index as i32
}

fn map_dev_err(err: DriverError) -> KError {
    match err {
        DriverError::AlreadyExists => KError::AlreadyExists,
        DriverError::WouldBlock => KError::WouldBlock,
        DriverError::InvalidInput => KError::InvalidInput,
        DriverError::Io => KError::Io,
        DriverError::Unsupported => KError::Unsupported,
        DriverError::ResourceBusy => KError::ResourceBusy,
        _ => KError::BadState,
    }
}
