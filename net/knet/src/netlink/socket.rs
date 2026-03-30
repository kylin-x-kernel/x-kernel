// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec, vec::Vec};
use core::{sync::atomic::Ordering, task::Context};

use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, PollSet, Pollable};

use super::{
    NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE, NetlinkAddr, NetlinkPacket, NetlinkRxQueue,
    NetlinkSocketInner, UEVENT_SEQNUM, UEVENT_SUBSCRIBERS,
    route::{build_error_response, handle_route_request},
};
use crate::{
    RecvOptions, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};

/// Minimal AF_NETLINK socket.
pub struct NetlinkSocket {
    pub(super) inner: Arc<NetlinkSocketInner>,
}

impl NetlinkSocket {
    pub fn new(protocol: i32) -> Self {
        Self {
            inner: Arc::new(NetlinkSocketInner {
                protocol,
                local_addr: ksync::RwLock::new(None),
                rx_queue: ksync::Mutex::new(NetlinkRxQueue::default()),
                poll_rx: Arc::new(PollSet::new()),
                general: crate::general::GeneralOptions::new(),
            }),
        }
    }

    fn local_addr_inner(&self) -> KResult<NetlinkAddr> {
        (*self.inner.local_addr.read()).ok_or(KError::NotConnected)
    }

    fn push_response(&self, packet: NetlinkPacket) -> KResult {
        if !self.inner.rx_queue.lock().push_back(packet) {
            return Err(LinuxError::ENOBUFS.into());
        }
        self.inner.poll_rx.wake();
        Ok(())
    }

    fn handle_request(&self, request: Vec<u8>) -> Vec<NetlinkPacket> {
        match self.inner.protocol {
            NETLINK_ROUTE => handle_route_request(&request),
            NETLINK_KOBJECT_UEVENT => Vec::new(),
            _ => vec![NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_error_response(&request, LinuxError::EPROTONOSUPPORT),
            }],
        }
    }

    fn update_uevent_subscription(&self, addr: NetlinkAddr) {
        if self.inner.protocol != NETLINK_KOBJECT_UEVENT {
            return;
        }
        let mut subs = UEVENT_SUBSCRIBERS.lock();
        subs.retain(|weak| {
            weak.upgrade()
                .is_some_and(|inner| !Arc::ptr_eq(&inner, &self.inner))
        });
        if addr.groups != 0 {
            subs.push(Arc::downgrade(&self.inner));
        }
    }
}

pub fn publish_kobject_uevent(group: u32, payload: &[u8]) {
    let seqnum = UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let mut payload_with_seqnum = Vec::with_capacity(payload.len() + 32);
    payload_with_seqnum.extend_from_slice(payload);
    payload_with_seqnum.extend_from_slice(format!("SEQNUM={seqnum}\0").as_bytes());

    let mut subs = UEVENT_SUBSCRIBERS.lock();
    subs.retain(|weak| {
        let Some(inner) = weak.upgrade() else {
            return false;
        };
        let Some(addr) = *inner.local_addr.read() else {
            return false;
        };
        if addr.groups & group != 0
            && inner.rx_queue.lock().push_back(NetlinkPacket {
                from: NetlinkAddr {
                    pid: 0,
                    groups: group,
                },
                data: payload_with_seqnum.clone(),
            })
        {
            inner.poll_rx.wake();
        }
        true
    });
}

impl Configurable for NetlinkSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<bool> {
        self.inner.general.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<bool> {
        self.inner.general.set_option_inner(opt)
    }
}

impl SocketOps for NetlinkSocket {
    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let addr = local_addr.into_netlink()?;
        *self.inner.local_addr.write() = Some(addr);
        self.update_uevent_subscription(addr);
        self.inner.poll_rx.wake();
        Ok(())
    }

    // TODO: Support netlink connect() semantics if future users rely on a
    // connected peer model. The current implementation only needs bind/send/recv.
    fn connect(&self, _remote_addr: SocketAddrEx) -> KResult {
        Err(KError::OperationNotSupported)
    }

    fn send(&self, mut src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        self.local_addr_inner()?;
        let mut request = Vec::with_capacity(src.remaining());
        src.read_to_end(&mut request)?;

        match options.to {
            Some(SocketAddrEx::Netlink(_addr)) => {}
            Some(_) => return Err(KError::InvalidInput),
            None => {}
        }

        for packet in self.handle_request(request.clone()) {
            self.push_response(packet)?;
        }

        Ok(request.len())
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions<'_>) -> KResult<usize> {
        self.inner.general.recv_poller(self, || {
            let mut rx_queue = self.inner.rx_queue.lock();
            let packet_len = rx_queue
                .front()
                .map(|packet| packet.data.len())
                .ok_or(KError::WouldBlock)?;
            if dst.remaining_mut() < packet_len {
                return Err(LinuxError::EMSGSIZE.into());
            }
            let packet = rx_queue.pop_front().ok_or(KError::WouldBlock)?;
            if let Some(from) = options.from.as_deref_mut() {
                *from = SocketAddrEx::Netlink(packet.from);
            }

            let written = dst.write(&packet.data)?;
            Ok(written)
        })
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        self.local_addr_inner().map(SocketAddrEx::Netlink)
    }

    // TODO: Support peer lookup if we add netlink connect() semantics.
    // The current implementation only tracks the local netlink address.
    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        Err(KError::OperationNotSupported)
    }

    // TODO: Define shutdown semantics if a future netlink use case needs them.
    // The current implementation models datagram-style request/response and
    // event delivery, so shutdown() is not supported.
    fn shutdown(&self, _how: Shutdown) -> KResult {
        Err(KError::OperationNotSupported)
    }
}

impl Pollable for NetlinkSocket {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if !self.inner.rx_queue.lock().is_empty() {
            events |= IoEvents::IN;
        }
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) && self.inner.rx_queue.lock().is_empty() {
            self.inner.poll_rx.register(context.waker());
        }
    }
}
