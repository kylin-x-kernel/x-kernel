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
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::AtomicU64;

use ksync::{Mutex, RwLock};
use lazyinit::LazyInit;
use smoltcp::wire::IpAddress;

use crate::{SERVICE, general::GeneralOptions};

mod route;
mod socket;
#[cfg(unittest)]
mod tests;
mod wire;

pub use route::{build_initial_state, init_route_state};
pub use socket::{NetlinkSocket, publish_kobject_uevent};
pub(crate) const RT_TABLE_MAIN: u8 = wire::route::TABLE_MAIN;
pub(crate) const RTN_UNICAST: u8 = wire::route::TYPE_UNICAST;

pub const NETLINK_ROUTE: i32 = 0;
pub(super) const NETLINK_KOBJECT_UEVENT: i32 = 15;
pub(super) const NLM_F_REQUEST: u16 = 0x0001;
pub(super) const NLM_F_ACK: u16 = 0x0004;
pub(super) const NLM_F_REPLACE: u16 = 0x0100;
pub(super) const NLM_F_EXCL: u16 = 0x0200;
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

// Reserved for future kernel-originated route protocol handling.
// pub(super) const RTPROT_KERNEL: u8 = 2;
// pub(super) const RT_SCOPE_NOWHERE: u8 = 255;
// pub(super) const RTM_NEWNEIGH_FAMILY: u8 = 0;

pub(super) const IFF_UP: u32 = 1 << 0;
pub(super) const IFF_BROADCAST: u32 = 1 << 1;
pub(super) const IFF_LOOPBACK: u32 = 1 << 3;
pub(super) const IFF_RUNNING: u32 = 1 << 6;
pub(super) const IFF_MULTICAST: u32 = 1 << 12;
pub(super) const IFF_LOWER_UP: u32 = 1 << 16;

pub(super) const ARPHRD_LOOPBACK: u16 = 772;
pub(super) const ARPHRD_ETHER: u16 = 1;

#[derive(Clone, Debug)]
pub struct LinkState {
    pub index: i32,
    pub name: String,
    pub flags: u32,
    pub mtu: u32,
    pub operstate: u8,
    pub link_type: u16,
    pub mac: [u8; 6],
    pub broadcast: [u8; 6],
}

#[derive(Clone, Copy, Debug)]
pub struct AddrState {
    pub index: u32,
    pub family: u8,
    pub prefix_len: u8,
    pub scope: u8,
    pub address: IpAddress,
}

#[derive(Clone, Copy, Debug)]
pub struct RouteState {
    pub family: u8,
    pub dst_len: u8,
    pub table: u8,
    pub protocol: u8,
    pub scope: u8,
    pub route_type: u8,
    pub oif: u32,
    pub dst: Option<IpAddress>,
    pub gateway: Option<IpAddress>,
    pub prefsrc: Option<IpAddress>,
}

#[derive(Clone, Copy, Debug)]
pub struct NeighState {
    pub family: u8,
    pub ifindex: u32,
    pub state: u16,
    pub flags: u8,
    pub dst: IpAddress,
    pub lladdr: Option<[u8; 6]>,
}

#[derive(Clone, Debug, Default)]
pub struct RtnetlinkState {
    pub links: Vec<LinkState>,
    pub addrs: Vec<AddrState>,
    pub routes: Vec<RouteState>,
    pub neighs: Vec<NeighState>,
}

pub(super) static ROUTE_STATE: LazyInit<RwLock<RtnetlinkState>> = LazyInit::new();
pub(super) static UEVENT_SUBSCRIBERS: Mutex<Vec<Weak<NetlinkSocketInner>>> = Mutex::new(Vec::new());
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
        if packet_len > NETLINK_RX_QUEUE_LIMIT
            || self.bytes.saturating_add(packet_len) > NETLINK_RX_QUEUE_LIMIT
        {
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
}

pub(super) struct NetlinkSocketInner {
    pub(super) protocol: i32,
    pub(super) local_addr: RwLock<Option<NetlinkAddr>>,
    pub(super) rx_queue: Mutex<NetlinkRxQueue>,
    pub(super) poll_rx: Arc<kpoll::PollSet>,
    pub(super) general: GeneralOptions,
}
