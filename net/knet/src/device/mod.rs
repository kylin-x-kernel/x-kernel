// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network device abstractions.
use alloc::string::String;

use ::core::task::Waker;
use kerrno::{KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError};
use ktime_types::MonotonicInstant;

mod ethernet;
mod loopback;
#[cfg(feature = "vsock")]
mod vsock;

pub use ethernet::*;
pub use loopback::*;
#[cfg(feature = "vsock")]
pub use vsock::*;

use crate::{
    buf::PacketBuf,
    ip::{IpAddress, Ipv4Cidr},
};

pub(crate) const LINK_FLAG_UP: u32 = 1 << 0;
pub(crate) const LINK_FLAG_BROADCAST: u32 = 1 << 1;
pub(crate) const LINK_FLAG_LOOPBACK: u32 = 1 << 3;
const LINK_FLAG_POINTOPOINT: u32 = 1 << 4;
pub(crate) const LINK_FLAG_RUNNING: u32 = 1 << 6;
const LINK_FLAG_MASTER: u32 = 1 << 10;
const LINK_FLAG_SLAVE: u32 = 1 << 11;
pub(crate) const LINK_FLAG_MULTICAST: u32 = 1 << 12;
pub(crate) const LINK_FLAG_LOWER_UP: u32 = 1 << 16;
const LINK_FLAG_DORMANT: u32 = 1 << 17;
const LINK_FLAG_ECHO: u32 = 1 << 18;
pub(crate) const LINK_FLAG_VOLATILE: u32 = LINK_FLAG_LOOPBACK
    | LINK_FLAG_POINTOPOINT
    | LINK_FLAG_BROADCAST
    | LINK_FLAG_ECHO
    | LINK_FLAG_MASTER
    | LINK_FLAG_SLAVE
    | LINK_FLAG_RUNNING
    | LINK_FLAG_LOWER_UP
    | LINK_FLAG_DORMANT;
pub(crate) const IF_OPER_UNKNOWN: u8 = 0;
pub(crate) const IF_OPER_DOWN: u8 = 2;
pub(crate) const IF_OPER_UP: u8 = 6;
pub(crate) const IPV4_MIN_MTU: usize = 68;
pub(crate) const ETHERNET_MAX_MTU: usize = crate::consts::STANDARD_MTU;
pub(crate) const LOOPBACK_MAX_MTU: usize = 65_536;
const INTERFACE_NAME_MAX_LEN: usize = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LinkKind {
    Loopback,
    Ethernet,
}

impl LinkKind {
    pub(crate) fn validate_mtu(self, mtu: usize) -> Result<(), LinuxError> {
        let max_mtu = match self {
            Self::Loopback => LOOPBACK_MAX_MTU,
            Self::Ethernet => ETHERNET_MAX_MTU,
        };
        (IPV4_MIN_MTU..=max_mtu)
            .contains(&mtu)
            .then_some(())
            .ok_or(LinuxError::EINVAL)
    }
}

#[derive(Debug)]
pub(crate) struct LinkConfigUpdate {
    pub(crate) name: Option<String>,
    pub(crate) mtu: Option<usize>,
    pub(crate) is_up: Option<bool>,
}

pub(crate) fn validate_interface_name(name: &str) -> Result<(), LinuxError> {
    let is_invalid = name.is_empty()
        || name.len() > INTERFACE_NAME_MAX_LEN
        || matches!(name, "." | "..")
        || name.bytes().any(|byte| {
            byte == b'/' || byte == b':' || byte == b'\x0b' || byte.is_ascii_whitespace()
        });
    (!is_invalid).then_some(()).ok_or(LinuxError::EINVAL)
}

#[derive(Clone, Debug)]
pub(crate) struct LinkSnapshot {
    pub(crate) ifindex: i32,
    pub(crate) name: String,
    pub(crate) flags: u32,
    pub(crate) mtu: usize,
    pub(crate) operstate: u8,
    pub(crate) kind: LinkKind,
    pub(crate) hardware_addr: [u8; 6],
    pub(crate) broadcast_addr: [u8; 6],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct LinkSendSnapshot {
    pub(crate) is_up: bool,
    pub(crate) mtu: usize,
    pub(crate) hardware_addr: [u8; 6],
}

/// Neighbor states accepted from the control plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NeighborState {
    Incomplete,
    Permanent { hardware_addr: [u8; 6] },
}

/// A control-plane neighbor update targeted at one network device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdate {
    pub(crate) dev: usize,
    pub(crate) dst: IpAddress,
    pub(crate) state: NeighborState,
}

/// Trait implemented by network device backends.
pub trait NetDevice: Send + Sync {
    fn name(&self) -> &str;

    /// Returns the link-layer kind without allocating a snapshot.
    fn link_kind(&self) -> LinkKind;

    /// Returns the configured IP-layer MTU.
    fn mtu(&self) -> usize;

    /// Returns whether the device can currently receive and transmit packets.
    fn is_link_up(&self) -> bool;

    /// Returns the link-layer state owned by this device.
    fn link_snapshot(&self, ifindex: i32) -> LinkSnapshot;

    /// Returns the allocation-free link state needed by packet transmission.
    fn link_send_snapshot(&self) -> LinkSendSnapshot;

    /// Device-model ID for driver-backed devices.
    fn device_id(&self) -> Option<kdevice::DeviceId> {
        None
    }

    /// Polls one received packet from the device.
    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> Option<PacketBuf>;

    /// Returns whether the device has RX work available without waiting.
    fn has_rx_work(&self) -> bool;

    /// Sends a validated IP packet to the next hop.
    ///
    /// `source_addr` is the source parsed by the Router before dispatch.
    /// Returns `true` if this operation resulted in the readiness of receive
    /// operation. This is true for loopback devices and can be used to speed
    /// up packet processing.
    fn send_ip_packet(
        &mut self,
        ifindex: i32,
        next_hop: IpAddress,
        source_addr: IpAddress,
        packet: PacketBuf,
        timestamp: MonotonicInstant,
    ) -> bool;

    /// Sends a link-layer frame through the device.
    ///
    /// `ifindex` identifies the egress network interface and `frame` contains
    /// the complete link-layer frame to transmit. Devices that do not support
    /// raw link-layer transmission keep the default implementation, which
    /// returns [`kerrno::KError::OperationNotSupported`].
    fn send_link_frame(&mut self, _ifindex: i32, _frame: &[u8]) -> KResult<usize> {
        Err(kerrno::KError::OperationNotSupported)
    }

    /// Registers the current RX wait with this device.
    ///
    /// Devices backed by a multi-waiter source register `context` directly.
    /// Single-waker devices retain `source_waker`, which fans out through the
    /// Service RX/timeout poll set.
    fn register_rx_waker(
        &self,
        source_waker: &Waker,
        context: &mut PollContext<'_>,
    ) -> Result<(), PollRegisterError>;

    /// Updates the user-visible interface name for this device.
    fn set_name(&mut self, _name: String) {}

    /// Updates the device MTU.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `mtu` is outside the backend's supported range.
    fn set_mtu(&mut self, _mtu: usize) -> Result<(), LinuxError> {
        Err(LinuxError::EOPNOTSUPP)
    }

    /// Updates the administrative link state.
    ///
    /// Backends stop receiving and transmitting packets while the link is down.
    fn set_link_up(&mut self, _is_up: bool) {}

    /// Replaces the IPv4 address projections prepared by the Router owner.
    ///
    /// `assigned_addrs` belongs to this device and drives egress broadcast
    /// handling. `local_addrs` contains every local address and drives weak-host
    /// ARP receive behavior.
    fn set_ipv4_addrs(&mut self, _assigned_addrs: &[Ipv4Cidr], _local_addrs: &[Ipv4Cidr]) {}

    /// Removes queued packets whose IPv4 source address is no longer local.
    fn remove_pending_ipv4_source(&mut self, _addr: crate::ip::Ipv4Address) {}

    /// Applies one neighbor-table update to this device.
    fn apply_neighbor_update(&mut self, _update: NeighborUpdate) -> Result<(), LinuxError> {
        Err(LinuxError::EOPNOTSUPP)
    }

    /// Returns whether this device owns a neighbor entry for `dst`.
    fn has_neighbor(&self, _dst: IpAddress) -> bool {
        false
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn ethernet_mtu_validation_accepts_only_supported_boundaries() {
        assert_eq!(LinkKind::Ethernet.validate_mtu(IPV4_MIN_MTU), Ok(()));
        assert_eq!(LinkKind::Ethernet.validate_mtu(ETHERNET_MAX_MTU), Ok(()));
        assert_eq!(
            LinkKind::Ethernet.validate_mtu(IPV4_MIN_MTU - 1),
            Err(LinuxError::EINVAL)
        );
        assert_eq!(
            LinkKind::Ethernet.validate_mtu(ETHERNET_MAX_MTU + 1),
            Err(LinuxError::EINVAL)
        );
    }

    #[def_test]
    fn interface_name_rejects_linux_whitespace_bytes() {
        assert_eq!(validate_interface_name("eth\x0b1"), Err(LinuxError::EINVAL));
    }
}
