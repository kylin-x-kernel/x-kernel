// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network device abstractions.
use alloc::{string::String, vec::Vec};

use ::core::task::Waker;
use kerrno::{KResult, LinuxError};
use kpoll::{PollContext, PollRegisterError};
use ktime_types::MonotonicInstant;

mod ethernet;
mod id;
mod loopback;
pub(crate) mod net_rx;
mod registry;

pub(crate) use ethernet::*;
pub(crate) use id::{NetDeviceId, RxDeviceSelection};
pub(crate) use loopback::*;
pub(crate) use registry::{DeviceRegistry, Ipv4AddrChange};

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
pub(crate) const ARPHRD_ETHER: u16 = 1;
pub(crate) const ARPHRD_LOOPBACK: u16 = 772;
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

#[derive(Clone, Debug, Eq, PartialEq)]
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

/// State of one neighbor-table entry.
///
/// `Reachable` is the state learned from address resolution; `Permanent` is
/// installed by the control plane and never ages out.  Keeping the state on
/// the entry is required because deletion notifications report the state that
/// actually existed, not a state inferred from whether a link-layer address is
/// present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NeighborState {
    Incomplete,
    Reachable { hardware_addr: [u8; 6] },
    Stale { hardware_addr: [u8; 6] },
    Failed,
    Permanent { hardware_addr: [u8; 6] },
}

impl NeighborState {
    /// Returns the link-layer address, or `None` while resolution is pending.
    pub(crate) const fn hardware_addr(self) -> Option<[u8; 6]> {
        match self {
            Self::Incomplete | Self::Failed => None,
            Self::Reachable { hardware_addr }
            | Self::Stale { hardware_addr }
            | Self::Permanent { hardware_addr } => Some(hardware_addr),
        }
    }
}

/// A control-plane neighbor update targeted at one network device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborUpdate {
    pub(crate) dev: NetDeviceId,
    pub(crate) dst: IpAddress,
    pub(crate) state: NeighborState,
}

/// A neighbor-table entry removed from a network device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NeighborSnapshot {
    pub(crate) dev: NetDeviceId,
    pub(crate) dst: IpAddress,
    pub(crate) state: NeighborState,
}

/// Asynchronous neighbor-table change owned by the data plane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NeighborEvent {
    Added(NeighborSnapshot),
    Removed(NeighborSnapshot),
}

/// Outcome charged to the Router's device RX budget.
#[derive(Debug)]
pub(crate) enum RxPoll {
    /// No work item was consumed by the budgeted receive step.
    /// Loopback may still have progressed its independent UDP fallback.
    Empty,
    /// One frame or deferred packet was consumed by the budgeted receive step.
    /// Ethernet ARP handling and dropped frames yield `None`.
    Consumed(Option<net_rx::RxPacket>),
}

/// Trait implemented by network device backends.
pub(crate) trait NetDevice: Send + Sync {
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
    #[cfg(unittest)]
    fn device_id(&self) -> Option<kdevice::DeviceId> {
        None
    }

    /// Exact class publication identity for driver-backed devices.
    fn publication_id(&self) -> Option<kdevice::PublicationId> {
        None
    }

    /// Polls one budgeted device receive work item.
    ///
    /// Ethernet consumes at most one driver frame per call, returning after
    /// handling or dropping it instead of searching for a deliverable packet.
    /// [`RxPoll::Consumed`] charges one unit of RX budget even when the frame
    /// produces no IP packet.
    ///
    /// Loopback first runs a separately bounded stamped-UDP fallback, whose
    /// round limit is derived from the queue length at entry. That work is
    /// outside the Router's device RX budget. It then polls at most one deferred
    /// packet, returning [`RxPoll::Consumed`] only when it removes one.
    /// [`RxPoll::Empty`] can therefore follow successful UDP fallback delivery.
    ///
    /// Receive or recycling failures end the caller's RX round through a
    /// separate error result.
    fn poll_rx(&mut self, ifindex: i32, timestamp: MonotonicInstant) -> KResult<RxPoll>;

    /// Returns whether the device has RX work available without waiting.
    fn has_rx_work(&self) -> bool;

    /// Returns the next time this device needs timer work even without a
    /// received frame.
    ///
    /// Ethernet uses this for neighbor expiry and unresolved-probe retries.
    /// The Service includes the earliest device deadline in its timer wakeup
    /// so an idle link still advances those state machines. A device whose
    /// poll path freezes timer work, such as an administratively down link,
    /// must report `None` rather than a deadline that cannot make progress.
    fn next_neighbor_deadline(&self) -> Option<MonotonicInstant> {
        None
    }

    /// Sends a validated IP packet to the next hop.
    ///
    /// The Router calls this in task context. Implementations may allocate and
    /// callers must not invoke it from hardirq, softirq, or BH-disabled context.
    ///
    /// `source_addr` is the source parsed by the Router before dispatch.
    /// Returns `true` when the send made RX ready, typically after a
    /// successful loopback enqueue. Returns `false` when it did not:
    /// Ethernet TX, or a drop such as link down, over-MTU, or a full
    /// `NetRx` queue. This is a poller hint, not a send-success flag.
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
    ///
    /// `can_override` is false when the request did not carry `NLM_F_REPLACE`.
    /// In that case an existing valid entry keeps its current link-layer
    /// address, matching Linux `__neigh_update`.
    fn apply_neighbor_update(
        &mut self,
        _update: NeighborUpdate,
        _can_override: bool,
    ) -> Result<(), LinuxError> {
        Err(LinuxError::EOPNOTSUPP)
    }

    /// Returns whether this device owns a neighbor entry for `dst`.
    fn has_neighbor(&self, _dst: IpAddress) -> bool {
        false
    }

    /// Returns the state of a neighbor entry.
    ///
    /// The outer option distinguishes a missing entry from an entry in the
    /// incomplete state, whose hardware address is not known yet.
    fn neighbor_state(&self, _dst: IpAddress) -> Option<NeighborState> {
        None
    }

    /// Returns every neighbor entry currently owned by this device.
    ///
    /// Device teardown snapshots these entries before destroying the device so
    /// subscribers receive one `RTM_DELNEIGH` per flushed neighbor.
    fn neighbor_snapshots(&self, _dev: NetDeviceId) -> Vec<NeighborSnapshot> {
        Vec::new()
    }

    /// Removes a neighbor entry and reports whether it existed.
    fn remove_neighbor(&mut self, _dst: IpAddress) -> bool {
        false
    }

    /// Drains neighbor changes produced by data-plane packet processing.
    ///
    /// Control-plane mutations return their own events to the caller, so this
    /// queue is reserved for asynchronous ARP learning and resolution.
    fn take_neighbor_events(&mut self) -> Vec<NeighborEvent> {
        Vec::new()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/device/mod/tests.rs");
