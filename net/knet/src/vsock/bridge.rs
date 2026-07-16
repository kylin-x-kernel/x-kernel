// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trusty-compatible vsock-TIPC bridge.
//!
//! The bridge is intentionally wired into the vsock device event router rather
//! than exposed as an AF_VSOCK endpoint. That lets it preserve virtio-vsock
//! record boundaries: one `Received(conn_id, len)` event becomes exactly one
//! TIPC message.

use alloc::{
    collections::{BTreeSet, VecDeque},
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};
use core::{
    future::poll_fn,
    str,
    sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    task::Poll,
    time::Duration,
};

use kclass::{ClassDevice, prelude::*};
use kerrno::{KError, KResult};
use klazy::Lazy;
use kpoll::PollSet;
use ksync::Mutex;
use ktask::future::block_on;
use tipc::{
    Handle, HandleEventMask, HandleSet, HandleSetCommand, HandleSetEntry, IPC_CHAN_MAX_BUF_SIZE,
    IPC_PORT_PATH_MAX, IpcChan, IpcConnectFlags, IpcMsgInfo, IpcPort, IpcPortFlags, IpcUuid,
    UEvent, ipc_port_connect_async, ipc_port_create, ipc_port_publish,
};

use super::{
    VsockAddr, VsockConnId,
    bridge_connection::{BridgeConnection, BridgeConnectionState},
    bridge_port_map::{BRIDGE_PORT_MAP, TIPC_TO_VSOCK_MAP, TipcToVsockMapping, bridge_mapping},
    connection_manager::{ConnectionState, VSOCK_CONN_MANAGER},
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
static NEXT_REVERSE_PORT: AtomicU32 = AtomicU32::new(0x8000);

#[derive(Debug)]
enum VsockBridgeEvent {
    ConnectionRequest(VsockConnId),
    Connected(VsockConnId),
    Received(VsockConnId, usize),
    Disconnected(VsockConnId),
    CreditUpdate(VsockConnId),
}

struct PublishedPort {
    port: Arc<IpcPort>,
    mapping_index: usize,
}

/// Initializes the bridge against the registered vsock device.
pub fn init(dev: ClassDevice<VsockDeviceImpl>) -> KResult {
    VSOCK_BRIDGE.init(dev)
}

/// Starts bridge worker tasks once the scheduler can spawn on every CPU.
pub fn start() {
    VSOCK_BRIDGE.start()
}

/// Routes a raw vsock device event to the bridge when it belongs to a bridged
/// port or to an existing bridged connection.
pub fn route_event(event: VsockDriverEventType) -> bool {
    VSOCK_BRIDGE.route_event(event)
}

struct VsockBridge {
    dev: Mutex<Option<ClassDevice<VsockDeviceImpl>>>,
    connections: Mutex<Vec<BridgeConnection>>,
    /// Inbound host ConnectionRequest seen by route_event but not yet processed
    /// by rx_task (BridgeConnection not created). Used so a same-batch Received
    /// (port 0 service name) routes here without claiming all mapped-port traffic.
    pending_inbound: Mutex<BTreeSet<VsockConnId>>,
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
            dev: Mutex::new(None),
            connections: Mutex::new(Vec::new()),
            pending_inbound: Mutex::new(BTreeSet::new()),
            rx_queue: Mutex::new(VecDeque::new()),
            rx_waiters: PollSet::new(),
            handle_set: HandleSet::handle_set_create(),
            ports: Mutex::new(Vec::new()),
            start_requested: AtomicBool::new(false),
            runtime_started: AtomicBool::new(false),
        }
    }

    fn init(&self, dev: ClassDevice<VsockDeviceImpl>) -> KResult {
        {
            let mut guard = self.dev.lock();
            if guard.is_some() {
                return Ok(());
            }
            *guard = Some(dev.clone());
        }

        for mapping in BRIDGE_PORT_MAP {
            dev.listen(mapping.port);
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
        if self.dev.lock().is_none() {
            return;
        }
        if self.runtime_started.swap(true, Ordering::SeqCst) {
            return;
        }
        ktask::spawn_with_name(rx_task, "vsock-tipc-rx".to_string());
        ktask::spawn_with_name(tx_task, "vsock-tipc-tx".to_string());
        crate::device::start_vsock_polling();
    }

    fn route_event(&self, event: VsockDriverEventType) -> bool {
        // vsock-poll may deliver ConnectionRequest and Received back-to-back while
        // rx_task has not yet created BridgeConnection. Track inbound CR in
        // pending_inbound so Received routes here without claiming every event on
        // BRIDGE_PORT_MAP local_port (which would break AF_VSOCK bind(1..4)+connect).
        let bridge_event = match event {
            VsockDriverEventType::ConnectionRequest(conn_id) => {
                if bridge_mapping(conn_id.local_port).is_none() {
                    return false;
                }
                self.mark_pending_inbound(conn_id);
                VsockBridgeEvent::ConnectionRequest(conn_id)
            }
            VsockDriverEventType::Received(conn_id, len) => {
                if !self.should_route_received(conn_id) {
                    return false;
                }
                VsockBridgeEvent::Received(conn_id, len)
            }
            VsockDriverEventType::Connected(conn_id) => {
                if !self.should_route_existing(conn_id) {
                    return false;
                }
                VsockBridgeEvent::Connected(conn_id)
            }
            VsockDriverEventType::Disconnected(conn_id) => {
                if !self.should_route_existing(conn_id) {
                    return false;
                }
                VsockBridgeEvent::Disconnected(conn_id)
            }
            VsockDriverEventType::CreditUpdate(conn_id) => {
                if !self.should_route_existing(conn_id) {
                    return false;
                }
                VsockBridgeEvent::CreditUpdate(conn_id)
            }
            VsockDriverEventType::Unknown => return false,
        };
        self.push_rx_event(bridge_event);
        true
    }

    fn mark_pending_inbound(&self, conn_id: VsockConnId) {
        self.pending_inbound.lock().insert(conn_id);
    }

    fn clear_pending_inbound(&self, conn_id: VsockConnId) {
        self.pending_inbound.lock().remove(&conn_id);
    }

    /// Same-batch port 0 service-name record: CR marked pending, Received follows.
    fn should_route_received(&self, conn_id: VsockConnId) -> bool {
        self.has_connection(conn_id) || self.pending_inbound.lock().contains(&conn_id)
    }

    /// Lifecycle events for an established or pending-inbound bridge connection.
    fn should_route_existing(&self, conn_id: VsockConnId) -> bool {
        self.has_connection(conn_id) || self.pending_inbound.lock().contains(&conn_id)
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

    fn pop_rx_event(&self) -> VsockBridgeEvent {
        block_on(poll_fn(|cx| {
            if let Some(event) = self.rx_queue.lock().pop_front() {
                Poll::Ready(event)
            } else {
                self.rx_waiters.register(cx.waker());
                Poll::Pending
            }
        }))
    }

    fn wait_handle_event(&self) -> UEvent {
        loop {
            match self.handle_set.poll_one() {
                Ok(Some(event)) => return event,
                Ok(None) | Err(KError::NotFound) => {
                    block_on(poll_fn(|cx| {
                        self.handle_set.register(cx, HandleEventMask::READY);
                        if self.handle_set.poll(false).contains(HandleEventMask::READY) {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    }));
                }
                Err(err) => {
                    warn!("vsock bridge handle-set poll failed: {err:?}");
                    ktask::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn dev(&self) -> KResult<ClassDevice<VsockDeviceImpl>> {
        self.dev.lock().as_ref().cloned().ok_or(KError::NotFound)
    }

    fn handle_rx_event(&self, event: VsockBridgeEvent) {
        match event {
            VsockBridgeEvent::ConnectionRequest(conn_id) => self.on_connection_request(conn_id),
            VsockBridgeEvent::Received(conn_id, len) => self.on_received(conn_id, len),
            VsockBridgeEvent::Connected(conn_id) => self.on_connected(conn_id),
            VsockBridgeEvent::Disconnected(conn_id) => self.close_connection(conn_id, true),
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
            self.clear_pending_inbound(conn_id);
            self.abort_vsock(conn.conn_id());
            return;
        }
        self.connections.lock().push(conn);
        self.clear_pending_inbound(conn_id);
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
                self.push_rx_event(VsockBridgeEvent::Received(conn_id, len));
                ktask::sleep(Duration::from_millis(10));
            }
            Some(_) => {
                warn!("vsock bridge received data in invalid state for {conn_id:?}");
                self.close_connection(conn_id, true);
            }
            None => {
                self.clear_pending_inbound(conn_id);
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
                    self.push_rx_event(VsockBridgeEvent::Received(conn_id, len));
                    return;
                }
                if let Err(err) = conn.stage_rx(&data).and_then(|_| conn.tipc_try_send()) {
                    warn!("vsock bridge failed to send vsock data to TIPC: {err:?}");
                    drop(conns);
                    self.close_connection(conn_id, true);
                }
            }
            Err(err) => {
                warn!("vsock bridge failed to read vsock data: {err:?}");
                self.close_connection(conn_id, true);
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
        let read_len = self.dev()?.recv(conn_id, &mut data).map_err(map_dev_err)?;
        if read_len != len {
            return Err(KError::Io);
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
                self.close_connection(conn_id, true);
            }
        }
    }

    fn reject_dynamic_connect(&self, conn_id: VsockConnId) {
        let should_reject = self
            .connections
            .lock()
            .iter()
            .any(|conn| conn.conn_id() == conn_id && conn.local_port() == 0);
        if should_reject
            && let Err(err) = self.dev().and_then(|dev| {
                dev.send(conn_id, &[DYNAMIC_CONNECT_STATUS_REJECT])
                    .map_err(map_dev_err)
            })
        {
            warn!("vsock bridge failed to send dynamic reject byte: {err:?}");
        }
        self.close_connection(conn_id, true);
    }

    fn close_connection(&self, conn_id: VsockConnId, disconnect_vsock: bool) {
        self.clear_pending_inbound(conn_id);
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
        VSOCK_CONN_MANAGER.lock().remove_connection(conn_id);
        if disconnect_vsock {
            let _ = self
                .dev()
                .and_then(|dev| dev.disconnect(conn_id).map_err(map_dev_err));
        }
    }

    fn abort_vsock(&self, conn_id: VsockConnId) {
        self.clear_pending_inbound(conn_id);
        let _ = self
            .dev()
            .and_then(|dev| dev.abort(conn_id).map_err(map_dev_err));
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
        let (local_port, conn_id) = reserve_reverse_connection(mapping.target_addr)?;
        let conn = BridgeConnection::new_tipc_only(
            token,
            mapping.target_addr,
            local_port,
            mapping.tipc_service,
            channel.clone(),
        );
        self.connections.lock().push(conn);
        if let Err(err) = self.dev()?.connect(conn_id).map_err(map_dev_err) {
            self.close_connection(conn_id, false);
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

        if event
            .event
            .intersects(HandleEventMask::HUP | HandleEventMask::ERROR)
        {
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
                self.close_connection(conn_id, true);
            }
            return;
        }
        if event.event.contains(HandleEventMask::READY) {
            self.on_tipc_ready(conn_id);
        }
        if event.event.contains(HandleEventMask::SEND_UNBLOCKED) {
            self.on_tipc_send_unblocked(conn_id);
        }
        if event.event.contains(HandleEventMask::MSG) {
            self.forward_tipc_to_vsock(conn_id);
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
        if send_dynamic_success
            && let Err(err) = self.dev().and_then(|dev| {
                dev.send(conn_id, &[DYNAMIC_CONNECT_STATUS_OK])
                    .map_err(map_dev_err)
            })
        {
            warn!("vsock bridge failed to send dynamic success byte: {err:?}");
            self.close_connection(conn_id, true);
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
            self.close_connection(conn_id, true);
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
                    self.close_connection(conn_id, true);
                    return;
                }
            };

            let result = self.read_tipc_msg(&channel, msg).and_then(|data| {
                channel.ipc_put_msg(msg.id)?;
                self.dev()?
                    .send(conn_id, &data)
                    .map_err(map_dev_err)
                    .and_then(|sent| {
                        if sent == data.len() {
                            Ok(sent)
                        } else {
                            Err(KError::Io)
                        }
                    })
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
                    self.close_connection(conn_id, true);
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
        let event = VSOCK_BRIDGE.pop_rx_event();
        VSOCK_BRIDGE.handle_rx_event(event);
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

fn reserve_reverse_connection(peer_addr: VsockAddr) -> KResult<(u32, VsockConnId)> {
    let local_cid = crate::device::vsock_guest_cid()?;
    let mut manager = VSOCK_CONN_MANAGER.lock();
    loop {
        let port = NEXT_REVERSE_PORT.fetch_add(1, Ordering::Relaxed);
        if bridge_mapping(port).is_some() || manager.is_local_port_in_use(port) {
            continue;
        }
        let conn_id = VsockConnId {
            peer_addr,
            local_port: port,
        };
        manager.create_connection(
            conn_id,
            VsockAddr {
                cid: local_cid,
                port,
            },
            Some(peer_addr),
            ConnectionState::Connecting,
        )?;
        return Ok((port, conn_id));
    }
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
