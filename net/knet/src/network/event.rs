// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Neutral notifications produced by successful network configuration changes.
//!
//! The network domain emits these values instead of constructing protocol
//! messages.  Each control-plane front end can then translate the same change
//! into its own representation, which keeps mutation state and event
//! publication from drifting apart.

use crate::{
    device::{LinkSnapshot, NeighborSnapshot, NeighborUpdate},
    network::{interface_address::Ipv4AddrSnapshot, router::Rule},
};

/// Who caused a route event.
///
/// The rtnetlink header carries the requesting port and sequence for routes
/// changed by a management request.  Routes derived from an address change are
/// built by the kernel FIB notifier and therefore have no requester metadata,
/// even when the address request itself used `NLM_F_ECHO`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RouteProvenance {
    /// The route change was requested directly by a management operation.
    Request,
    /// The kernel derived the route change from another state transition.
    Kernel,
}

/// A network state transition observed by the control plane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NetworkEvent {
    /// An interface's flags, name, MTU, or operational state changed.
    LinkChanged {
        link: LinkSnapshot,
        changed_flags: u32,
    },
    /// An interface left the network stack.
    LinkRemoved { link: LinkSnapshot },
    /// An IPv4 address was assigned to an interface.
    AddrAdded { addr: Ipv4AddrSnapshot },
    /// An IPv4 address was removed from an interface.
    AddrRemoved { addr: Ipv4AddrSnapshot },
    /// A configured route was inserted.
    RouteAdded {
        route: Rule,
        provenance: RouteProvenance,
    },
    /// A configured route was removed.
    RouteRemoved {
        route: Rule,
        provenance: RouteProvenance,
    },
    /// A neighbor entry was created or updated.
    NeighborAdded { neighbor: NeighborUpdate },
    /// A neighbor entry was removed.
    NeighborRemoved { neighbor: NeighborSnapshot },
}
