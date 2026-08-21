// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AF_NETLINK sockets, kobject uevent delivery, and a limited rtnetlink
//! protocol front end.
//!
//! Link, address, route, and neighbor objects are owned by devices and the
//! Router. This module parses requests, serializes mutations with
//! [`rtnl_lock`], and dumps live snapshots.

use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::AtomicU64;

use ksync::{Mutex, MutexGuard, RwLock, static_lock};

use crate::general::GeneralOptions;

mod rtnetlink;
mod socket;
#[cfg(unittest)]
mod tests;
mod wire;

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

static_lock! {
    pub(super) static RTNETLINK_MUTATION_LOCK: Mutex<()> = Mutex::new(());
}

/// Serializes rtnetlink mutations with device unregister.
///
/// Linux keeps this semaphore in `net/core/rtnetlink.c` as `rtnl_mutex` and
/// exports `rtnl_lock()`. Device teardown in `net/core/dev.c` takes the same
/// lock through `unregister_netdev()`.
pub(crate) fn rtnl_lock() -> MutexGuard<'static, ()> {
    RTNETLINK_MUTATION_LOCK.lock()
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
