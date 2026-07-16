// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-connection state for the Trusty-compatible vsock-TIPC bridge.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
    vec,
};

use kerrno::{KError, KResult};
use tipc::{IPC_CHAN_MAX_BUF_SIZE, IpcChan};

use super::{VsockAddr, VsockConnId};

/// Twice the TIPC channel buffer so that pending messages can be buffered
/// when TIPC blocks, matching Trusty's vsock bridge design.
pub const BRIDGE_RX_BUFFER_SIZE: usize = 2 * IPC_CHAN_MAX_BUF_SIZE;

/// Bridge lifecycle states, matching Trusty's vsock bridge state machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BridgeConnectionState {
    /// Entry is not usable.
    #[expect(dead_code)]
    Invalid,
    /// Host-side vsock exists, but no TIPC channel has been attached yet.
    VsockOnly,
    /// TIPC channel exists, but the outbound vsock connection is pending.
    TipcOnly,
    /// TIPC connect has been started and is waiting for READY.
    TipcConnecting,
    /// One vsock packet is staged while TIPC send backpressure is active.
    TipcSendBlocked,
    /// Both endpoints are connected and data may flow in either direction.
    Active,
    /// TIPC side closed while vsock cleanup is in progress.
    #[expect(dead_code)]
    TipcClosed,
    /// Connection has been closed.
    #[expect(dead_code)]
    Closed,
}

/// One bridged vsock/TIPC connection.
pub struct BridgeConnection {
    /// Stable bridge-local token used as TIPC handle-set cookie.
    token: usize,
    /// Host peer address.
    peer: VsockAddr,
    /// Guest local vsock port.
    local_port: u32,
    /// Current bridge lifecycle state.
    state: BridgeConnectionState,
    tipc_port_name: Option<String>,
    tipc_channel: Option<Arc<IpcChan>>,
    rx_buffer: Box<[u8]>,
    rx_pending: usize,
    rx_bytes: usize,
    tx_bytes: usize,
}

impl BridgeConnection {
    /// Creates a host-originated connection entry.
    pub fn new(token: usize, peer: VsockAddr, local_port: u32) -> Self {
        Self {
            token,
            peer,
            local_port,
            state: BridgeConnectionState::VsockOnly,
            tipc_port_name: None,
            tipc_channel: None,
            rx_buffer: vec![0; BRIDGE_RX_BUFFER_SIZE].into_boxed_slice(),
            rx_pending: 0,
            rx_bytes: 0,
            tx_bytes: 0,
        }
    }

    /// Creates a TIPC-originated connection entry.
    pub fn new_tipc_only(
        token: usize,
        peer: VsockAddr,
        local_port: u32,
        tipc_port_name: &str,
        channel: Arc<IpcChan>,
    ) -> Self {
        let mut conn = Self::new(token, peer, local_port);
        conn.set_state(BridgeConnectionState::TipcOnly);
        conn.set_tipc_channel(tipc_port_name, channel);
        conn
    }

    /// Returns the stable bridge-local token used as TIPC handle-set cookie.
    pub fn token(&self) -> usize {
        self.token
    }

    /// Returns the guest local vsock port.
    pub fn local_port(&self) -> u32 {
        self.local_port
    }

    /// Returns the current bridge lifecycle state.
    pub fn state(&self) -> BridgeConnectionState {
        self.state
    }

    /// Updates the current bridge lifecycle state.
    pub fn set_state(&mut self, state: BridgeConnectionState) {
        self.state = state;
    }

    /// Returns the driver connection identifier for this bridge entry.
    pub fn conn_id(&self) -> VsockConnId {
        VsockConnId {
            peer_addr: self.peer,
            local_port: self.local_port,
        }
    }

    /// Returns the attached TIPC channel.
    pub fn channel(&self) -> KResult<Arc<IpcChan>> {
        self.tipc_channel.clone().ok_or(KError::NotConnected)
    }

    /// Attaches a TIPC channel to this connection.
    pub fn set_tipc_channel(&mut self, name: &str, channel: Arc<IpcChan>) {
        self.tipc_port_name = Some(name.to_string());
        self.tipc_channel = Some(channel);
    }

    /// Stages one complete vsock packet for delivery to TIPC.
    pub fn stage_rx(&mut self, data: &[u8]) -> KResult {
        if data.len() > self.rx_buffer.len() {
            return Err(KError::OutOfRange);
        }
        self.rx_buffer[..data.len()].copy_from_slice(data);
        self.rx_pending = data.len();
        Ok(())
    }

    /// Attempts to send the staged packet to TIPC.
    pub fn tipc_try_send(&mut self) -> KResult {
        if self.rx_pending == 0 {
            self.set_state(BridgeConnectionState::Active);
            return Ok(());
        }
        let channel = self.channel()?;
        match channel.ipc_send_msg(&self.rx_buffer[..self.rx_pending]) {
            Ok(len) if len == self.rx_pending => {
                self.rx_bytes += len;
                self.rx_pending = 0;
                self.set_state(BridgeConnectionState::Active);
                Ok(())
            }
            Ok(_) => Err(KError::Io),
            Err(KError::WouldBlock) => {
                self.set_state(BridgeConnectionState::TipcSendBlocked);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// Returns whether a vsock packet is staged.
    pub fn has_pending_rx(&self) -> bool {
        self.rx_pending != 0
    }

    /// Accounts one message sent from TIPC to vsock.
    pub fn add_tx_bytes(&mut self, len: usize) {
        self.tx_bytes += len;
    }
}
