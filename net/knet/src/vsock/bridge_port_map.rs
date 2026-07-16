// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Static Trusty-compatible vsock-TIPC bridge port mappings.

use tipc::IpcUuid;

use super::VsockAddr;

/// Maps one host-visible vsock port to a local TIPC service.
pub(crate) struct BridgePortMapping {
    /// Host-visible local vsock port.
    pub(crate) port: u32,
    /// Target TIPC service. Empty means dynamic port 0 handshake.
    pub(crate) tipc_service: &'static str,
}

/// Maps a local TIPC forwarding service to a host vsock endpoint.
pub(crate) struct TipcToVsockMapping {
    /// TIPC service created by the bridge.
    pub(crate) tipc_service: &'static str,
    /// Host vsock endpoint to connect to.
    pub(crate) target_addr: VsockAddr,
    /// Allowed TIPC client UUIDs. Empty means all clients are allowed.
    pub(crate) allowed_uuids: &'static [IpcUuid],
}

/// Host-to-TA bridge ports.
pub(crate) const BRIDGE_PORT_MAP: &[BridgePortMapping] = &[
    BridgePortMapping {
        port: 0,
        tipc_service: "",
    },
    BridgePortMapping {
        port: 1,
        tipc_service: "com.android.trusty.keymint",
    },
    BridgePortMapping {
        port: 2,
        tipc_service: "com.android.trusty.gatekeeper",
    },
    BridgePortMapping {
        port: 3,
        tipc_service: "com.android.trusty.vsock.forwarder",
    },
    BridgePortMapping {
        port: 4,
        tipc_service: "com.android.trusty.widevine.transact",
    },
];

/// TA-to-host forwarding services.
pub(crate) const TIPC_TO_VSOCK_MAP: &[TipcToVsockMapping] = &[TipcToVsockMapping {
    tipc_service: "com.android.trusty.vsock.forwarder",
    target_addr: VsockAddr { cid: 2, port: 0 },
    allowed_uuids: &[],
}];

/// Returns whether `port` is reserved for the bridge.
pub(crate) fn is_bridge_port(port: u32) -> bool {
    BRIDGE_PORT_MAP.iter().any(|mapping| mapping.port == port)
}

/// Returns the host-to-TA mapping for `port`.
pub(crate) fn bridge_mapping(port: u32) -> Option<&'static BridgePortMapping> {
    BRIDGE_PORT_MAP.iter().find(|mapping| mapping.port == port)
}
