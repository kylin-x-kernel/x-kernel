// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AF_NETLINK sockets, kobject uevent delivery, and a limited rtnetlink
//! protocol front end.
//!
//! Link, address, route, and neighbor objects are owned by devices and the
//! Router. This module parses requests, serializes mutations with the shared
//! network configuration lock, and dumps live snapshots.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use kerrno::LinuxError;
use ksync::{Mutex, RwLock};

use crate::{general::GeneralOptions, network::event::NetworkEvent};

mod port;
mod rtnetlink;
mod socket;
unittest::path_mod_test!(tests, "../../ut-tests/netlink/tests.rs");
mod wire;

pub use socket::{NetlinkSocket, NetlinkSocketType, publish_kobject_uevent};
pub(crate) use wire::route::{
    PROTOCOL_BOOT as RTPROT_BOOT, PROTOCOL_KERNEL as RTPROT_KERNEL, SCOPE_HOST as RT_SCOPE_HOST,
    SCOPE_LINK as RT_SCOPE_LINK, SCOPE_UNIVERSE as RT_SCOPE_UNIVERSE,
    TABLE_LOCAL as RT_TABLE_LOCAL, TABLE_MAIN as RT_TABLE_MAIN, TYPE_BROADCAST as RTN_BROADCAST,
    TYPE_LOCAL as RTN_LOCAL, TYPE_MULTICAST as RTN_MULTICAST, TYPE_UNICAST as RTN_UNICAST,
};

pub const NETLINK_ROUTE: i32 = 0;
pub(super) const NETLINK_USERSOCK: i32 = 2;
pub(super) const NETLINK_AUDIT: i32 = 9;
pub(super) const NETLINK_KOBJECT_UEVENT: i32 = 15;
/// Number of netlink protocol slots, matching Linux `MAX_LINKS`.
pub const NETLINK_MAX_LINKS: i32 = 32;
/// Multicast groups this netlink implementation reports for every protocol.
///
/// `NETLINK_ADD_MEMBERSHIP`/`NETLINK_DROP_MEMBERSHIP` are bounded by the
/// protocol's `ngroups` in `netlink_setsockopt`; the group mask kept on the
/// socket address is 32 bits wide, so the accepted range is 1..=32.
pub(super) const NETLINK_GROUP_COUNT: i32 = 32;
/// Protocols accepted by `socket(AF_NETLINK, ...)`. `NETLINK_AUDIT` has no
/// kernel handler here; it is a sink so audit emitters (systemd-resolved) can
/// open their audit socket and send best-effort messages.
pub fn netlink_protocol_supported(protocol: i32) -> bool {
    matches!(
        protocol,
        NETLINK_ROUTE | NETLINK_USERSOCK | NETLINK_AUDIT | NETLINK_KOBJECT_UEVENT
    )
}
pub(super) const NLM_F_REQUEST: u16 = 0x0001;
pub(super) const NLM_F_ACK: u16 = 0x0004;
pub(super) const NLM_F_ECHO: u16 = 0x0008;
pub(super) const NLM_F_DUMP: u16 = 0x0300;
pub(super) const NLM_F_DUMP_FILTERED: u16 = 0x0020;
pub(super) const NLM_F_REPLACE: u16 = 0x0100;
pub(super) const NLM_F_EXCL: u16 = 0x0200;
pub(super) const NLM_F_CREATE: u16 = 0x0400;
pub(super) const NLM_F_MULTI: u16 = 0x0002;
/// Set on a response-only `NLMSG_ERROR` whose original request payload is
/// omitted; Linux uses the same bit as request-side `NLM_F_REPLACE`.
pub(super) const NLM_F_CAPPED: u16 = 0x0100;
pub(super) const NLMSG_ERROR: u16 = 0x0002;
pub(super) const NLMSG_DONE: u16 = 0x0003;
pub(super) const NLMSG_HDR_LEN: usize = 16;
/// First message type reserved for protocol handlers.  Types below this are
/// netlink control messages and never reach a protocol callback.
pub(super) const NLMSG_MIN_TYPE: u16 = 0x10;
pub(super) const NLMSG_ALIGNTO: usize = 4;

pub(super) const RTM_NEWLINK: u16 = 16;
pub(super) const RTM_DELLINK: u16 = 17;
pub(super) const RTM_GETLINK: u16 = 18;
#[cfg(unittest)]
pub(super) const RTM_SETLINK: u16 = 19;
pub(super) const RTM_NEWADDR: u16 = 20;
pub(super) const RTM_DELADDR: u16 = 21;
pub(super) const RTM_GETADDR: u16 = 22;
pub(super) const RTM_NEWROUTE: u16 = 24;
pub(super) const RTM_DELROUTE: u16 = 25;
pub(super) const RTM_GETROUTE: u16 = 26;
pub(super) const RTM_NEWNEIGH: u16 = 28;
pub(super) const RTM_DELNEIGH: u16 = 29;
pub(super) const RTM_GETNEIGH: u16 = 30;

/// First rtnetlink message type and the highest type the kernel dispatches.
pub(super) const RTM_BASE: u16 = 16;
pub(super) const RTM_MAX: u16 = 123;
/// `rtnl_msgtype_kind()` selects the low two bits of `type - RTM_BASE`.
pub(super) const RTNL_KIND_MASK: u16 = 3;
pub(super) const RTNL_KIND_GET: u16 = 2;

// pub(super) const RT_SCOPE_NOWHERE: u8 = 255;
// pub(super) const RTM_NEWNEIGH_FAMILY: u8 = 0;

/// Multicast group numbers used by rtnetlink notifications.  These are
/// one-based; `NetlinkAddr::groups` stores the corresponding zero-based bit
/// mask (`1 << (group - 1)`).
pub(super) const RTNLGRP_LINK: u32 = 1;
pub(super) const RTNLGRP_NEIGH: u32 = 3;
pub(super) const RTNLGRP_IPV4_IFADDR: u32 = 5;
pub(super) const RTNLGRP_IPV4_ROUTE: u32 = 7;
/// Reverse-path multicast groups that `rtnetlink_bind` restricts to
/// `CAP_NET_ADMIN`.
pub(super) const RTNLGRP_IPV4_MROUTE_R: u32 = 30;
pub(super) const RTNLGRP_IPV6_MROUTE_R: u32 = 31;

pub(super) use crate::device::{ARPHRD_ETHER, ARPHRD_LOOPBACK};

/// Maximum size of a kernel uevent datagram, including the appended
/// `SEQNUM=<n>\0` suffix.
pub(super) const UEVENT_BUFFER_SIZE: usize = 2048;
pub(super) static UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);
/// Linux initializes a netlink socket's `sk_rcvbuf` from
/// `net.core.rmem_default`, which is 212992 bytes.
pub(super) const NETLINK_RX_QUEUE_LIMIT: usize = 212_992;
/// The modeled `net.core.rmem_max` used to clamp `SO_RCVBUF` hints.
///
/// Linux initializes `sysctl_rmem_max` to 4 MiB while `sysctl_rmem_default`
/// remains 212992 bytes.
pub(super) const NETLINK_RMEM_MAX: usize = 4 << 20;
/// Minimum effective `sk_rcvbuf`, matching `SOCK_MIN_RCVBUF`.
pub(super) const NETLINK_RCVBUF_MIN: usize = 2_304;
/// Linux initializes a netlink socket's `sk_sndbuf` from
/// `net.core.wmem_default`, which is 212992 bytes.
pub(super) const NETLINK_SNDBUF: usize = 212_992;
/// The modeled `net.core.wmem_max` used to clamp `SO_SNDBUF` hints.
pub(super) const NETLINK_WMEM_MAX: usize = 4 << 20;
/// Minimum effective `sk_sndbuf`, matching `SOCK_MIN_SNDBUF`.
pub(super) const NETLINK_SNDBUF_MIN: usize = 4_608;
/// `netlink_sendmsg` reserves this much of `sk_sndbuf` for bookkeeping.
pub(super) const NETLINK_SNDBUF_RESERVED: usize = 32;

/// Delivers a kernel-originated rtnetlink notification to the sockets that
/// subscribed to `group`.
pub(super) fn publish_route_event(group: u32, data: &[u8]) {
    port::publish(NETLINK_ROUTE, group, 0, data, None, None, None);
}

/// Delivers a kernel-originated rtnetlink notification while excluding one
/// requester port from multicast, as `nlmsg_notify(..., report = true)` does
/// for `NLM_F_ECHO`.
pub(super) fn publish_route_event_excluding(group: u32, data: &[u8], exclude_pid: Option<u32>) {
    port::publish(NETLINK_ROUTE, group, 0, data, None, None, exclude_pid);
}

/// Publishes neutral network changes as kernel-originated rtnetlink
/// notifications.
pub(crate) fn publish_network_events(events: Vec<NetworkEvent>) {
    rtnetlink::publish_network_events(events);
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(C)]
pub struct NetlinkAddr {
    pub pid: u32,
    pub groups: u32,
}

#[derive(Clone)]
pub(super) struct NetlinkPacket {
    pub(super) from: NetlinkAddr,
    pub(super) data: Vec<u8>,
    /// Sender identity carried in the skb control block.
    ///
    /// `Some` for datagrams sent by a user process, including a validated
    /// explicit `SCM_CREDENTIALS` claim. `None` means kernel origin, whose
    /// control block is zeroed by `alloc_skb` in Linux and is reported as
    /// pid/uid/gid 0. Kernel-generated uevents are the one exception and carry
    /// an explicit root identity.
    pub(super) credentials: Option<crate::SenderCredentials>,
}

pub(super) struct NetlinkRxQueue {
    packets: VecDeque<NetlinkPacket>,
    bytes: usize,
    /// Effective `sk_rcvbuf`; Linux accounts netlink receive payload against
    /// this limit, not against the number of queued datagrams.
    limit: usize,
}

impl NetlinkRxQueue {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            bytes: 0,
            limit,
        }
    }

    pub(super) fn limit(&self) -> usize {
        self.limit
    }

    pub(super) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
    }

    pub(super) fn push_back(&mut self, packet: NetlinkPacket) -> bool {
        if !self.can_push(packet.data.len()) {
            return false;
        }
        self.bytes += packet.data.len();
        self.packets.push_back(packet);
        true
    }

    pub(super) fn pop_front(&mut self) -> Option<NetlinkPacket> {
        let packet = self.packets.pop_front()?;
        self.bytes = self.bytes.saturating_sub(packet.data.len());
        Some(packet)
    }

    pub(super) fn front(&self) -> Option<&NetlinkPacket> {
        self.packets.front()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }

    /// Whether a datagram of `len` bytes may be queued.
    ///
    /// Linux `netlink_attachskb` and `netlink_broadcast_deliver` add the
    /// incoming `skb->truesize` to `sk_rmem_alloc` before checking it against
    /// `sk_rcvbuf`.  The only exception is an empty queue, which accepts one
    /// datagram regardless of its size (`rmem == skb->truesize`).  Once any
    /// payload is queued, a datagram is admitted only when the resulting total
    /// stays within the effective limit.  Keeping the incoming length in the
    /// admission check is also required for blocking unicast: an oversized
    /// datagram waits until the queue is empty instead of being admitted
    /// against a non-empty queue and pushing accounting arbitrarily past the
    /// limit.
    pub(super) fn can_push(&self, len: usize) -> bool {
        self.packets.is_empty() || len <= self.limit.saturating_sub(self.bytes)
    }
}

pub(super) struct NetlinkSocketInner {
    pub(super) protocol: i32,
    /// The socket type the socket was created with, reported by `SO_TYPE` the
    /// way Linux keeps it in `sk_type`.
    pub(super) socket_type: NetlinkSocketType,
    pub(super) local_addr: RwLock<Option<NetlinkAddr>>,
    /// Multicast groups this socket subscribed to, one bit per zero-based
    /// group (`nlk->groups`).
    ///
    /// Kept apart from `local_addr` because `NETLINK_ADD_MEMBERSHIP` is valid
    /// before `bind()`: Linux only allocates the bitmap there
    /// (`netlink_realloc_groups`), it does not require a port id. Storing the
    /// subscription on the bound address discarded it for an unbound socket.
    pub(super) groups: AtomicU32,
    /// The peer stored by `connect()`.
    ///
    /// `Some(NetlinkAddr { pid: 0, groups: 0 })` is a connected socket whose
    /// destination is the kernel; `None` means unconnected.  The group mask is
    /// normalised to the lowest requested group, matching Linux's one-based
    /// `dst_group` and the value `getpeername()` reports.
    pub(super) remote_addr: RwLock<Option<NetlinkAddr>>,
    pub(super) send_lock: Mutex<()>,
    pub(super) rx_queue: Mutex<NetlinkRxQueue>,
    /// Effective `sk_sndbuf`; `SO_SNDBUF` stores the doubled, clamped value.
    pub(super) send_buffer: AtomicUsize,
    pub(super) poll_rx: Arc<kpoll::PollSet>,
    /// Wakes a unicast sender waiting for receive-queue space.
    pub(super) poll_tx: Arc<kpoll::PollSet>,
    /// Set when the receiving socket is released.  Blocked senders retry the
    /// port lookup, so release is observed as `ECONNREFUSED` (or as the
    /// replacement socket) rather than `EPIPE`.
    pub(super) closed: AtomicBool,
    /// `NETLINK_CAP_ACK`: cap the request echo in error acknowledgements.
    pub(super) cap_ack: AtomicBool,
    /// Receive-queue congestion, matching `NETLINK_S_CONGESTED`.
    ///
    /// It is set by the first dropped delivery and cleared only when the
    /// receive queue becomes empty.  While set, every delivery path must stop
    /// enqueueing even though byte accounting might otherwise permit it.
    pub(super) congested: AtomicBool,
    /// Pending socket error (`sk_err`), stored as a positive errno.
    ///
    /// Netlink uses this for `ENOBUFS` after an overrun.  `SO_ERROR` reads and
    /// clears the value but deliberately leaves [`Self::congested`] set.
    pub(super) socket_error: AtomicI32,
    pub(super) general: GeneralOptions,
}

impl NetlinkSocketInner {
    pub(super) fn is_congested(&self) -> bool {
        self.congested.load(Ordering::Acquire)
    }

    /// Records an overrun and reports `ENOBUFS` to the socket owner.
    ///
    /// Linux treats only the first transition as an edge: later drops while
    /// the socket remains congested do not create another pending error or
    /// wake-up.  Congestion is cleared separately when the queue drains.
    pub(super) fn overrun(&self) {
        // The acquire half of this swap pairs with the release clear in
        // `recv`, so a new congestion edge starts after the drain that
        // disarmed the previous edge is visible.  The release half publishes
        // the edge to `is_congested`'s acquire load.
        if self.congested.swap(true, Ordering::AcqRel) {
            return;
        }
        // Publish the pending errno to the acquire loads in `poll` and
        // `take_socket_error`.  A plain store would let those observers read
        // the new error without the preceding edge publication.
        self.socket_error
            .store(LinuxError::ENOBUFS.into_raw(), Ordering::Release);
        self.poll_rx.wake();
    }

    pub(super) fn take_socket_error(&self) -> i32 {
        // The acquire half observes the release store in `overrun`; the
        // release half clears the value so a concurrent overrun cannot lose
        // its edge behind an earlier reader.  `SO_ERROR` therefore reports a
        // complete errno even when it races with a new overrun.
        self.socket_error.swap(0, Ordering::AcqRel)
    }
}
