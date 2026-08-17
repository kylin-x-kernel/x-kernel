// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal netlink socket support for in-kernel consumers.
//!
//! This provides a small AF_NETLINK socket that is sufficient to grow a
//! rtnetlink implementation for Kata. The current dispatcher intentionally keeps
//! the protocol surface small and returns netlink errors for unsupported
//! requests, so later work can extend request handling without changing the
//! socket plumbing.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::AtomicU64;

use ksync::{Mutex, RwLock, static_lock};
use lazyinit::LazyInit;
use smoltcp::wire::IpAddress;

use crate::{SERVICE, general::GeneralOptions};

mod rtnetlink;
mod socket;
#[cfg(unittest)]
mod tests;
mod wire;

pub(crate) use rtnetlink::{build_initial_state, init_route_state, remove_device_state};
pub use socket::{NetlinkSocket, publish_kobject_uevent};
pub(crate) use wire::route::{
    PROTOCOL_BOOT as RTPROT_BOOT, PROTOCOL_KERNEL as RTPROT_KERNEL, SCOPE_HOST as RT_SCOPE_HOST,
    SCOPE_LINK as RT_SCOPE_LINK, SCOPE_UNIVERSE as RT_SCOPE_UNIVERSE, TABLE_MAIN as RT_TABLE_MAIN,
    TYPE_UNICAST as RTN_UNICAST,
};

pub const NETLINK_ROUTE: i32 = 0;
pub(super) const NETLINK_KOBJECT_UEVENT: i32 = 15;
pub(super) const NLM_F_REQUEST: u16 = 0x0001;
pub(super) const NLM_F_ACK: u16 = 0x0004;
pub(super) const NLM_F_REPLACE: u16 = 0x0100;
pub(super) const NLM_F_EXCL: u16 = 0x0200;
#[cfg(unittest)]
pub(super) const NLM_F_CREATE: u16 = 0x0400;
pub(super) const NLM_F_MULTI: u16 = 0x0002;
pub(super) const NLMSG_ERROR: u16 = 0x0002;
pub(super) const NLMSG_DONE: u16 = 0x0003;
pub(super) const NLMSG_HDR_LEN: usize = 16;
pub(super) const NLMSG_ALIGNTO: usize = 4;

pub(super) const RTM_NEWLINK: u16 = 16;
// Reserved for future RTM_DELLINK support.
// pub(super) const RTM_DELLINK: u16 = 17;
pub(super) const RTM_GETLINK: u16 = 18;
pub(super) const RTM_NEWADDR: u16 = 20;
pub(super) const RTM_DELADDR: u16 = 21;
pub(super) const RTM_GETADDR: u16 = 22;
pub(super) const RTM_NEWROUTE: u16 = 24;
pub(super) const RTM_DELROUTE: u16 = 25;
pub(super) const RTM_GETROUTE: u16 = 26;
pub(super) const RTM_NEWNEIGH: u16 = 28;

// pub(super) const RT_SCOPE_NOWHERE: u8 = 255;
// pub(super) const RTM_NEWNEIGH_FAMILY: u8 = 0;

pub(super) const ARPHRD_LOOPBACK: u16 = 772;
pub(super) const ARPHRD_ETHER: u16 = 1;

#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteState {
    pub(crate) family: u8,
    pub(crate) dst_len: u8,
    pub(crate) table: u8,
    pub(crate) protocol: u8,
    pub(crate) scope: u8,
    pub(crate) route_type: u8,
    pub(crate) oif: u32,
    pub(crate) dst: Option<IpAddress>,
    pub(crate) gateway: Option<IpAddress>,
    pub(crate) prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug)]
#[expect(dead_code)]
pub(crate) struct NeighState {
    pub(crate) family: u8,
    pub(crate) ifindex: u32,
    pub(crate) state: u16,
    pub(crate) flags: u8,
    pub(crate) dst: IpAddress,
    pub(crate) lladdr: Option<[u8; 6]>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RtnetlinkState {
    pub(crate) routes: Vec<RouteState>,
    pub(crate) neighs: Vec<NeighState>,
}

pub(super) static ROUTE_STATE: LazyInit<RwLock<RtnetlinkState>> = LazyInit::new();
static_lock! {
    pub(super) static RTNETLINK_MUTATION_LOCK: Mutex<()> = Mutex::new(());
}
static_lock! {
    pub(super) static UEVENT_SUBSCRIBERS: Mutex<Vec<Weak<NetlinkSocketInner>>> = Mutex::new(Vec::new());
}
pub(super) static UEVENT_SEQNUM: AtomicU64 = AtomicU64::new(0);
pub(super) const NETLINK_RX_QUEUE_LIMIT: usize = 64 * 1024;

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
}

#[derive(Default)]
pub(super) struct NetlinkRxQueue {
    packets: VecDeque<NetlinkPacket>,
    bytes: usize,
}

impl NetlinkRxQueue {
    pub(super) fn push_back(&mut self, packet: NetlinkPacket) -> bool {
        let packet_len = packet.data.len();
        if !self.can_push_bytes(packet_len) {
            return false;
        }
        self.bytes += packet_len;
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

    pub(super) fn can_push_bytes(&self, bytes: usize) -> bool {
        bytes <= NETLINK_RX_QUEUE_LIMIT
            && self.bytes.saturating_add(bytes) <= NETLINK_RX_QUEUE_LIMIT
    }
}

pub(super) struct NetlinkSocketInner {
    pub(super) protocol: i32,
    pub(super) local_addr: RwLock<Option<NetlinkAddr>>,
    pub(super) send_lock: Mutex<()>,
    pub(super) rx_queue: Mutex<NetlinkRxQueue>,
    pub(super) poll_rx: Arc<kpoll::PollSet>,
    pub(super) general: GeneralOptions,
}
