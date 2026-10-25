// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec, vec::Vec};
use core::sync::atomic::Ordering;

use kcred::Cred;
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};

use super::{
    NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE, NLMSG_HDR_LEN, NetlinkAddr, NetlinkPacket,
    NetlinkRxQueue, NetlinkSocketInner, UEVENT_SEQNUM, UEVENT_SUBSCRIBERS,
    rtnetlink::{
        build_error_response, handle_rtnetlink_request, rtnetlink_request_requires_privilege,
    },
    wire::{NlMsgHeader, align},
};
use crate::{
    ConnectOptions, RecvFlags, RecvOptions, SendOptions, Shutdown, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
};

/// The socket type requested at creation, kept because `netlink_create` accepts
/// both and Linux reports the stored `sk_type` back from `SO_TYPE`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NetlinkSocketType {
    Raw,
    Datagram,
}

impl NetlinkSocketType {
    /// The `SOCK_*` value `SO_TYPE` reports for this type.
    pub const fn as_raw(self) -> i32 {
        match self {
            Self::Raw => linux_raw_sys::net::SOCK_RAW as i32,
            Self::Datagram => linux_raw_sys::net::SOCK_DGRAM as i32,
        }
    }
}

/// Minimal AF_NETLINK socket.
pub struct NetlinkSocket {
    pub(super) inner: Arc<NetlinkSocketInner>,
}

impl NetlinkSocket {
    /// Creates an AF_NETLINK socket that reports `socket_type` from `SO_TYPE`.
    pub fn new(protocol: i32, socket_type: NetlinkSocketType) -> Self {
        Self {
            inner: Arc::new(NetlinkSocketInner {
                protocol,
                socket_type,
                local_addr: ksync::RwLock::new(None),
                send_lock: ksync::Mutex::new(()),
                rx_queue: ksync::Mutex::new(NetlinkRxQueue::default()),
                poll_rx: Arc::new(PollSet::new()),
                general: crate::general::GeneralOptions::new(),
            }),
        }
    }

    /// Returns whether this socket receives kernel object uevents.
    pub fn is_kobject_uevent(&self) -> bool {
        self.inner.protocol == NETLINK_KOBJECT_UEVENT
    }

    fn local_addr_inner(&self) -> KResult<NetlinkAddr> {
        (*self.inner.local_addr.read()).ok_or(KError::NotConnected)
    }

    fn handle_request(&self, request: &[u8], is_privileged: bool) -> Vec<NetlinkPacket> {
        match self.inner.protocol {
            NETLINK_ROUTE if route_request_requires_privilege(request) && !is_privileged => {
                vec![NetlinkPacket {
                    from: NetlinkAddr { pid: 0, groups: 0 },
                    data: build_error_response(request, LinuxError::EPERM),
                }]
            }
            NETLINK_ROUTE => handle_rtnetlink_request(request),
            NETLINK_KOBJECT_UEVENT => Vec::new(),
            _ => vec![NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_error_response(request, LinuxError::EPROTONOSUPPORT),
            }],
        }
    }

    pub(crate) fn send_with_cred(
        &self,
        src: impl Read + IoBuf,
        options: SendOptions,
        cred: &Cred,
    ) -> KResult<usize> {
        self.send_inner(src, options, cred.is_privileged())
    }

    fn send_inner(
        &self,
        mut src: impl Read + IoBuf,
        options: SendOptions,
        is_privileged: bool,
    ) -> KResult<usize> {
        self.local_addr_inner()?;
        let mut request = Vec::with_capacity(src.remaining());
        src.read_to_end(&mut request)?;

        match options.to {
            Some(SocketAddrEx::Netlink(addr))
                if self.inner.protocol == NETLINK_ROUTE && (addr.pid != 0 || addr.groups != 0) =>
            {
                return Err(LinuxError::EOPNOTSUPP.into());
            }
            Some(SocketAddrEx::Netlink(_)) => {}
            Some(_) => return Err(KError::InvalidInput),
            None => {}
        }

        let _send_guard = self.inner.send_lock.lock();

        if self.inner.protocol == NETLINK_ROUTE {
            // Match Linux netlink_rcv_skb: stop at a short or truncated header
            // without failing the send or rolling back earlier messages.
            let requests = split_route_requests(&request);
            if requests.is_empty() {
                return Ok(request.len());
            }
            let has_mutation = requests
                .iter()
                .any(|request| route_request_requires_privilege(request));
            if has_mutation
                && requests
                    .iter()
                    .any(|request| !route_request_requires_privilege(request))
            {
                return Err(LinuxError::EOPNOTSUPP.into());
            }

            // send_lock keeps the preflight valid through response enqueue.
            // Preflight capacity under rx_queue only, then drop that lock before
            // handle_request takes control-plane state locks. The ordering is
            // send_lock, control-plane state, then rx_queue.
            if has_mutation {
                let queue = self.inner.rx_queue.lock();
                let reserved_bytes = requests.iter().try_fold(0usize, |total, request| {
                    total.checked_add(max_error_response_len(request))
                });
                if !reserved_bytes.is_some_and(|bytes| queue.can_push_bytes(bytes)) {
                    return Err(LinuxError::ENOBUFS.into());
                }
            }

            let mut responses = Vec::new();
            for request in requests {
                responses.extend(self.handle_request(request, is_privileged));
            }
            self.push_responses(responses)?;
        } else {
            let responses = self.handle_request(&request, is_privileged);
            self.push_responses(responses)?;
        }

        Ok(request.len())
    }

    fn push_responses(&self, mut responses: Vec<NetlinkPacket>) -> KResult {
        responses.retain(|packet| !packet.data.is_empty());
        let mut queue = self.inner.rx_queue.lock();
        let response_bytes = responses
            .iter()
            .try_fold(0usize, |total, packet| total.checked_add(packet.data.len()));
        if !response_bytes.is_some_and(|bytes| queue.can_push_bytes(bytes)) {
            return Err(LinuxError::ENOBUFS.into());
        }
        let has_responses = !responses.is_empty();
        for packet in responses {
            let pushed = queue.push_back(packet);
            debug_assert!(pushed, "netlink response capacity was prevalidated");
        }
        drop(queue);
        if has_responses {
            self.inner.poll_rx.wake();
        }
        Ok(())
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
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        self.inner.general.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        self.inner.general.set_option_inner(opt)
    }
}

impl SocketOps for NetlinkSocket {
    /// Returns the `SO_TYPE` reported for this socket.
    fn socket_type(&self) -> i32 {
        self.inner.socket_type.as_raw()
    }

    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        let addr = local_addr.into_netlink()?;
        if self.inner.protocol == NETLINK_ROUTE && addr.groups != 0 {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        *self.inner.local_addr.write() = Some(addr);
        self.update_uevent_subscription(addr);
        self.inner.poll_rx.wake();
        Ok(())
    }

    // TODO: Support netlink connect() semantics if future users rely on a
    // connected peer model. The current implementation only needs bind/send/recv.
    fn connect(&self, _remote_addr: SocketAddrEx, _options: ConnectOptions) -> KResult {
        Err(KError::OperationNotSupported)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        // Credential-free callers cannot establish authority. Kernel callers
        // performing rtnetlink mutations must use send_with_cred explicitly.
        self.send_inner(src, options, false)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions<'_>) -> KResult<usize> {
        self.inner
            .general
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let mut rx_queue = self.inner.rx_queue.lock();
                let packet = rx_queue.front().ok_or(KError::WouldBlock)?;
                let packet_len = packet.data.len();
                if let Some(from) = options.from.as_deref_mut() {
                    *from = SocketAddrEx::Netlink(packet.from);
                }
                if self.is_kobject_uevent()
                    && self.inner.general.pass_credentials()
                    && let Some(ancillary) = options.ancillary.as_deref_mut()
                {
                    ancillary.push(Arc::new(crate::KernelAncillaryData::Credentials(
                        crate::options::UnixCredentials {
                            pid: 0,
                            uid: 0,
                            gid: 0,
                        },
                    )));
                }

                let write_len = packet_len.min(dst.remaining_mut());
                dst.write_all(&packet.data[..write_len])?;
                if write_len < packet_len
                    && let Some(out_flags) = options.out_flags.as_deref_mut()
                {
                    *out_flags |= RecvFlags::TRUNCATE;
                }
                if !options.flags.contains(RecvFlags::PEEK) {
                    let _ = rx_queue.pop_front();
                }

                Ok(if options.flags.contains(RecvFlags::TRUNCATE) {
                    packet_len
                } else {
                    write_len
                })
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

fn route_request_requires_privilege(request: &[u8]) -> bool {
    let Some(header) = NlMsgHeader::read(request) else {
        return false;
    };
    rtnetlink_request_requires_privilege(header.msg_type)
}

/// Splits a NETLINK_ROUTE datagram into complete netlink messages.
///
/// Mirrors Linux `netlink_rcv_skb`: iterate while at least one header fits,
/// stop when `nlmsg_len` is shorter than a header or longer than the remaining
/// bytes, and ignore any trailing padding without requiring it to be zero.
fn split_route_requests(mut datagram: &[u8]) -> Vec<&[u8]> {
    let mut requests = Vec::new();
    while datagram.len() >= NLMSG_HDR_LEN {
        let message_len =
            u32::from_ne_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]) as usize;
        if message_len < NLMSG_HDR_LEN || message_len > datagram.len() {
            break;
        }
        requests.push(&datagram[..message_len]);
        let aligned_len = align(message_len).min(datagram.len());
        datagram = &datagram[aligned_len..];
    }
    requests
}

fn max_error_response_len(request: &[u8]) -> usize {
    align(NLMSG_HDR_LEN + core::mem::size_of::<i32>() + request.len())
}

impl Pollable for NetlinkSocket {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if !self.inner.rx_queue.lock().is_empty() {
            events |= IoEvents::IN;
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) && self.inner.rx_queue.lock().is_empty() {
            context.register(&self.inner.poll_rx)?;
        }
        Ok(())
    }
}
