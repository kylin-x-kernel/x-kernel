// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use kcred::{CAP_NET_ADMIN, CAP_SYS_ADMIN, Cred};
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};

use super::{
    NETLINK_AUDIT, NETLINK_GROUP_COUNT, NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE, NLM_F_ACK,
    NLM_F_REQUEST, NLMSG_HDR_LEN, NLMSG_MIN_TYPE, NetlinkAddr, NetlinkPacket, NetlinkRxQueue,
    NetlinkSocketInner, UEVENT_SEQNUM, port,
    rtnetlink::{
        build_ack_response, build_error_response_with_cap, handle_rtnetlink_request_with_cred,
        normalize_reply_ports,
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

/// Checks a network capability in the current network namespace's owner.
fn net_capable(cred: &Cred, cap: u32) -> bool {
    kprocess::try_current_user_process()
        .and_then(|process| process.net_ns().ok())
        .map_or_else(
            || cred.has_cap(cap),
            |namespace| cred.has_cap_in(namespace.owner(), cap),
        )
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
                groups: core::sync::atomic::AtomicU32::new(0),
                remote_addr: ksync::RwLock::new(None),
                send_lock: ksync::Mutex::new(()),
                rx_queue: ksync::Mutex::new(NetlinkRxQueue::new(super::NETLINK_RX_QUEUE_LIMIT)),
                send_buffer: core::sync::atomic::AtomicUsize::new(super::NETLINK_SNDBUF),
                poll_rx: Arc::new(PollSet::new()),
                poll_tx: Arc::new(PollSet::new()),
                closed: core::sync::atomic::AtomicBool::new(false),
                cap_ack: core::sync::atomic::AtomicBool::new(false),
                congested: core::sync::atomic::AtomicBool::new(false),
                socket_error: core::sync::atomic::AtomicI32::new(0),
                general: crate::general::GeneralOptions::new(),
            }),
        }
    }

    /// Returns whether this socket receives kernel object uevents.
    pub fn is_kobject_uevent(&self) -> bool {
        self.inner.protocol == NETLINK_KOBJECT_UEVENT
    }

    /// Whether `NETLINK_ADD_MEMBERSHIP` is admitted without CAP_NET_ADMIN.
    ///
    /// `netlink_setsockopt` gates the membership commands on
    /// `netlink_allowed(sock, NL_CFG_F_NONROOT_RECV)`, which is a per-protocol
    /// table flag: `NETLINK_ROUTE` and `NETLINK_KOBJECT_UEVENT` set it, and
    /// `NETLINK_USERSOCK` sets only `NL_CFG_F_NONROOT_SEND`. This reuses the
    /// table's own `allows_unprivileged_receive` so the two policies cannot
    /// drift.
    pub fn allows_unprivileged_group_subscribe(&self) -> bool {
        super::port::allows_unprivileged_receive(self.inner.protocol)
    }

    fn local_addr_inner(&self) -> KResult<NetlinkAddr> {
        let mut addr = self.inner.local_addr.read().unwrap_or_default();
        // `getsockname` reports `nlk->groups`, which `NETLINK_ADD/DROP_MEMBERSHIP`
        // updates after `bind()`; the stored address keeps the bind-time value.
        addr.groups = self.inner.groups.load(Ordering::Relaxed);
        Ok(addr)
    }

    fn handle_request(
        &self,
        request: &[u8],
        can_route_mutate: bool,
        requesting_uid: u32,
    ) -> Vec<NetlinkPacket> {
        match self.inner.protocol {
            NETLINK_ROUTE => handle_rtnetlink_request_with_cred(
                request,
                self.inner.local_addr.read().unwrap_or_default().pid,
                can_route_mutate,
                requesting_uid,
                self.inner.cap_ack.load(Ordering::Relaxed),
            ),
            // Other protocols only exchange datagrams between user sockets.
            // A datagram addressed to port 0 is consumed by a protocol's
            // kernel socket, if it has one.
            _ => Vec::new(),
        }
    }

    /// Handles a datagram addressed to the `NETLINK_KOBJECT_UEVENT` kernel
    /// port.
    ///
    /// Linux runs `netlink_rcv_skb` over the datagram and calls
    /// `uevent_net_rcv_skb` once per request message.  Each callback
    /// re-broadcasts the *remaining* datagram without its netlink header, adds
    /// a `SEQNUM=<n>\0` suffix, and marks the source as port 0.  The callback
    /// requires `CAP_SYS_ADMIN`; failures are reported as netlink error ACKs
    /// while the enclosing send itself still succeeds.
    fn handle_kobject_uevent(
        &self,
        datagram: &[u8],
        can_uevent: bool,
        credentials: Option<&crate::SenderCredentials>,
    ) -> Vec<NetlinkPacket> {
        let requesting_port = self.inner.local_addr.read().unwrap_or_default().pid;
        let mut responses = Vec::new();
        let mut offset = 0usize;
        while datagram.len().saturating_sub(offset) >= NLMSG_HDR_LEN {
            let remaining = &datagram[offset..];
            let message_len =
                u32::from_ne_bytes([remaining[0], remaining[1], remaining[2], remaining[3]])
                    as usize;
            if message_len < NLMSG_HDR_LEN || message_len > remaining.len() {
                break;
            }
            let Some(header) = NlMsgHeader::read(&remaining[..message_len]) else {
                break;
            };
            let callback = header.flags & NLM_F_REQUEST != 0 && header.msg_type >= NLMSG_MIN_TYPE;
            let mut error = None;
            if callback {
                if !can_uevent {
                    error = Some(LinuxError::EPERM);
                } else {
                    // The sequence number is allocated before the size check,
                    // matching `uevent_net_broadcast`.
                    let seqnum = UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
                    let suffix = format!("SEQNUM={seqnum}\0");
                    if remaining.len() + suffix.len() > super::UEVENT_BUFFER_SIZE {
                        error = Some(LinuxError::EINVAL);
                    } else {
                        let mut payload = Vec::with_capacity(remaining.len() + suffix.len());
                        payload.extend_from_slice(&remaining[NLMSG_HDR_LEN..]);
                        payload.extend_from_slice(suffix.as_bytes());
                        port::publish(
                            NETLINK_KOBJECT_UEVENT,
                            1,
                            0,
                            &payload,
                            credentials,
                            None,
                            None,
                        );
                    }
                }
            }

            if let Some(error) = error {
                responses.push(normalize_reply_port(
                    build_error_response_with_cap(
                        remaining,
                        error,
                        self.inner.cap_ack.load(Ordering::Relaxed),
                    ),
                    requesting_port,
                ));
            } else if header.flags & NLM_F_ACK != 0 {
                responses.push(normalize_reply_port(
                    build_ack_response(remaining),
                    requesting_port,
                ));
            }

            offset += align(message_len).min(remaining.len());
        }
        responses
    }

    pub(crate) fn send_with_cred(
        &self,
        src: impl Read + IoBuf,
        options: SendOptions,
        sender_cred: &Cred,
    ) -> KResult<usize> {
        self.send_inner(
            src,
            options,
            net_capable(sender_cred, CAP_NET_ADMIN),
            net_capable(sender_cred, CAP_SYS_ADMIN),
            sender_cred.ruid(),
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: netlink send authorization and wire encoding form one \
                  ordered path; splitting it would separate the policy checks from the send"
    )]
    fn send_inner(
        &self,
        mut src: impl Read + IoBuf,
        options: SendOptions,
        sender_can_route_mutate: bool,
        sender_can_uevent: bool,
        requesting_uid: u32,
    ) -> KResult<usize> {
        // Linux `netlink_sendmsg` rejects out-of-band requests before the
        // zero-length check, destination parsing, or an implicit bind.
        if options.flags.contains(crate::SendFlags::OOB) {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        if src.remaining() == 0 {
            // Linux rejects an empty datagram with ENODATA before parsing the
            // destination or performing an implicit bind.
            return Err(LinuxError::ENODATA.into());
        }

        // `nl_groups` selects the lowest subscribed group for both the
        // broadcast and the metadata of the unicast copy.  A nonzero
        // destination requires NL_CFG_F_NONROOT_SEND on the socket protocol.
        let has_explicit_destination = options.to.is_some();
        let explicit_destination = match options.to {
            Some(SocketAddrEx::Netlink(addr)) => Some(addr),
            None => None,
            Some(_) => return Err(KError::InvalidInput),
        };
        let opener_can_route_mutate = options
            .opener_cred
            .as_ref()
            .map_or(sender_can_route_mutate, |cred| {
                net_capable(cred, CAP_NET_ADMIN)
            });
        let opener_can_uevent = options
            .opener_cred
            .as_ref()
            .map_or(sender_can_uevent, |cred| net_capable(cred, CAP_SYS_ADMIN));
        // Linux authorises the destination supplied in `msg_name`; a connected
        // destination was authorised by `connect()` and is not rechecked
        // against the credentials of each later send.
        if let Some(destination) = explicit_destination
            && (destination.pid != 0 || destination.groups != 0)
            && !sender_can_route_mutate
            && !port::allows_unprivileged_send(self.inner.protocol)
        {
            return Err(LinuxError::EPERM.into());
        }
        // `__netlink_ns_capable()` requires both the opener and the current
        // sender to hold the capability unless the skb carries an explicit
        // destination (`NETLINK_SKB_DST`).  An explicit PID 0 still sets that
        // flag, so only an addressless or connected send needs the opener.
        // The two kernel protocols use different capabilities: rtnetlink
        // requires `CAP_NET_ADMIN`, while the uevent callback requires
        // `CAP_SYS_ADMIN`.
        let kernel_can_route_mutate =
            sender_can_route_mutate && (has_explicit_destination || opener_can_route_mutate);
        let kernel_can_uevent =
            sender_can_uevent && (has_explicit_destination || opener_can_uevent);
        // The socket lock serialises `bind`, `connect`, and send. In
        // particular, a connected socket's destination, the multicast source
        // port, and its group mask must form one consistent snapshot. Reading
        // `remote_addr` outside this lock could send to a peer installed by a
        // concurrent `connect()` even though that connection had already been
        // replaced when the lock was acquired.
        let _send_guard = self.inner.send_lock.lock();
        let destination = explicit_destination
            .unwrap_or_else(|| self.inner.remote_addr.read().unwrap_or_default());
        let multicast_group =
            (destination.groups != 0).then(|| destination.groups.trailing_zeros() + 1);
        let current = *self.inner.local_addr.read();
        let local_addr = match current {
            Some(addr) => addr,
            None => {
                let addr = port::bind(&self.inner, NetlinkAddr::default(), None, false)?;
                *self.inner.local_addr.write() = Some(addr);
                addr
            }
        };
        let send_buffer = self.inner.send_buffer.load(Ordering::Relaxed);
        if src.remaining() > send_buffer.saturating_sub(super::NETLINK_SNDBUF_RESERVED) {
            return Err(LinuxError::EMSGSIZE.into());
        }
        let mut request = Vec::with_capacity(src.remaining());
        src.read_to_end(&mut request)?;

        if let Some(group) = multicast_group {
            port::publish(
                self.inner.protocol,
                group,
                local_addr.pid,
                &request,
                options.sender_credentials.as_ref(),
                Some(&self.inner),
                (destination.pid != 0).then_some(destination.pid),
            );
        }

        let destination_groups = multicast_group.map_or(0, |group| 1u32 << (group - 1));
        if destination.pid != 0 {
            port::deliver_unicast(
                self.inner.protocol,
                destination.pid,
                local_addr.pid,
                destination_groups,
                &request,
                options.sender_credentials.as_ref(),
                options.flags.nonblocking(),
            )?;
            return Ok(request.len());
        }

        if self.inner.protocol == NETLINK_ROUTE {
            // Match Linux netlink_rcv_skb: stop at a short or truncated header
            // without failing the send or rolling back earlier messages.
            let mut responses = Vec::new();
            for request in split_netlink_messages(&request) {
                responses.extend(self.handle_request(
                    request,
                    kernel_can_route_mutate,
                    requesting_uid,
                ));
            }
            self.push_responses(responses);
        } else if self.inner.protocol == NETLINK_KOBJECT_UEVENT {
            // The uevent receive callback uses the same `netlink_ns_capable`
            // rule as rtnetlink: an addressless send requires the capability
            // in both the opener and the current sender.
            let responses = self.handle_kobject_uevent(
                &request,
                kernel_can_uevent,
                options.sender_credentials.as_ref(),
            );
            self.push_responses(responses);
        } else if self.inner.protocol == NETLINK_AUDIT {
            // This kernel has no audit subsystem, and Linux's `audit_receive` is
            // void: the send succeeds and `audit_receive_msg`'s error travels
            // back as an `NLMSG_ERROR` ack, emitted here through the same
            // `build_error_response` path rtnetlink uses. Answering success
            // without the ack is what stalls a caller that set `NLM_F_ACK`, so
            // the ack is the part that must exist, not the errno.
            let responses = self.push_audit_unsupported(&request);
            self.push_responses(responses);
        } else {
            // Linux always ends `netlink_sendmsg` with a unicast to the
            // destination port, including port 0. Protocols without a kernel
            // socket therefore report ECONNREFUSED for a broadcast-only send.
            return Err(LinuxError::ECONNREFUSED.into());
        }

        Ok(request.len())
    }

    /// Enqueues generated replies on a best-effort basis.
    ///
    /// Linux sends ACKs and dump replies with `MSG_DONTWAIT` to the requesting
    /// port.  A full receive queue drops that individual reply while the send
    /// still succeeds and any mutation has already taken effect.  In
    /// particular there is no aggregate reservation that could turn a full
    /// reply queue into `ENOBUFS` before running the request.
    /// Builds the `NLMSG_ERROR` replies a message to the (absent) audit
    /// subsystem earns.
    ///
    /// Every message type is unimplemented, which is `EINVAL` in
    /// `audit_receive_msg`, and an error ack is reported whether or not the
    /// caller asked for one (`audit_receive`: "send an ack if the user asked for
    /// one ... or if there was an error").
    fn push_audit_unsupported(&self, request: &[u8]) -> Vec<NetlinkPacket> {
        let mut responses = Vec::new();
        let mut remaining = request;
        while let Some(header) = NlMsgHeader::read(remaining) {
            let message_len = header.len as usize;
            if message_len < NLMSG_HDR_LEN || message_len > remaining.len() {
                break;
            }
            responses.push(NetlinkPacket {
                from: NetlinkAddr { pid: 0, groups: 0 },
                data: build_error_response_with_cap(
                    remaining,
                    LinuxError::EINVAL,
                    self.inner.cap_ack.load(Ordering::Relaxed),
                ),
                credentials: None,
            });
            let step = align(message_len).min(remaining.len());
            if step == 0 {
                break;
            }
            remaining = &remaining[step..];
        }
        // `netlink_ack` stamps the reply with the sender's port id
        // (`NETLINK_CB(skb).portid`), not with the `nlmsg_pid` the sender wrote
        // into its request, so a sender that left that field zero can still
        // match the ack to its socket.
        normalize_reply_ports(
            responses,
            self.local_addr_inner().map_or(0, |addr| addr.pid),
        )
    }

    fn push_responses(&self, responses: Vec<NetlinkPacket>) {
        let mut queue = self.inner.rx_queue.lock();
        let mut pushed_any = false;
        let mut overflowed = false;
        for packet in responses {
            if packet.data.is_empty() {
                continue;
            }
            // Congestion is checked under the queue lock: `recv` clears it
            // while holding that lock after the queue drains, so a reply can
            // never slip into a congested queue between the two operations.
            if self.inner.is_congested() || !queue.push_back(packet) {
                overflowed = true;
            } else {
                pushed_any = true;
            }
        }
        drop(queue);
        if pushed_any {
            self.inner.poll_rx.wake();
        }
        if overflowed {
            self.inner.overrun();
        }
    }

    /// Binds using an explicit credential for group-subscription
    /// authorization.
    ///
    /// `rtnetlink_bind` gates the reverse-path multicast groups on
    /// `CAP_NET_ADMIN`; keeping the credential explicit makes that check
    /// testable and avoids consulting ambient task state in the registry.
    pub(crate) fn bind_with_cred(&self, local_addr: SocketAddrEx, cred: &Cred) -> KResult {
        self.bind_inner(local_addr, net_capable(cred, CAP_NET_ADMIN))
    }

    /// Connects using the credential active at the syscall boundary.
    ///
    /// `netlink_connect` consults the protocol's `NL_CFG_F_NONROOT_SEND`
    /// policy for a nonzero destination, with `CAP_NET_ADMIN` as the override.
    pub(crate) fn connect_with_cred(&self, remote_addr: SocketAddrEx, cred: &Cred) -> KResult {
        self.connect_inner(remote_addr, net_capable(cred, CAP_NET_ADMIN))
    }

    fn connect_inner(&self, remote_addr: SocketAddrEx, can_net_admin: bool) -> KResult {
        let destination = match remote_addr {
            SocketAddrEx::Unspecified => {
                // Linux `AF_UNSPEC` disconnects without checking the requested
                // length beyond `sa_family`, authority, or local bind state.
                // Serialise with send so an address-less send observes either
                // the old peer or the disconnected state, never a half-updated
                // connection.
                let _send_guard = self.inner.send_lock.lock();
                *self.inner.remote_addr.write() = None;
                return Ok(());
            }
            SocketAddrEx::Netlink(addr) => addr,
            // `netlink_connect` reports EINVAL for every other family, not
            // EAFNOSUPPORT as the generic address conversion would.
            _ => return Err(KError::InvalidInput),
        };

        if (destination.pid != 0 || destination.groups != 0)
            && !can_net_admin
            && !port::allows_unprivileged_send(self.inner.protocol)
        {
            return Err(LinuxError::EPERM.into());
        }

        // Serialise with bind and send.  A connected socket must keep the same
        // local port for the lifetime of the connection, and senders must
        // observe either the old or the new peer atomically.
        let _send_guard = self.inner.send_lock.lock();
        if self.inner.local_addr.read().is_none() {
            let local = port::bind(&self.inner, NetlinkAddr::default(), None, false)?;
            *self.inner.local_addr.write() = Some(local);
        }

        // Linux stores `ffs(nl_groups)`, so only the lowest group is selected.
        // Normalising here makes the stored peer exactly what `getpeername`
        // reports and what an address-less `sendmsg` must use.
        let groups = if destination.groups == 0 {
            0
        } else {
            1u32 << destination.groups.trailing_zeros()
        };
        *self.inner.remote_addr.write() = Some(NetlinkAddr {
            pid: destination.pid,
            groups,
        });
        Ok(())
    }

    fn bind_inner(&self, local_addr: SocketAddrEx, can_net_admin: bool) -> KResult {
        let requested = local_addr.into_netlink()?;
        // Serialise against send autobind and multicast source selection.
        let _send_guard = self.inner.send_lock.lock();
        let current = *self.inner.local_addr.read();
        let bound = port::bind(&self.inner, requested, current, can_net_admin)?;
        // `netlink_bind` *replaces* the low 32 bits of `nlk->groups` with the
        // requested `nl_groups` (`nlk->groups[0] = (old & ~0xffffffffUL) |
        // groups`), so a rebind drops subscriptions taken earlier rather than
        // adding to them.
        self.inner.groups.store(bound.groups, Ordering::Relaxed);
        *self.inner.local_addr.write() = Some(bound);
        self.inner.poll_rx.wake();
        Ok(())
    }
}

pub fn publish_kobject_uevent(group: u32, payload: &[u8]) {
    let seqnum = UEVENT_SEQNUM.fetch_add(1, Ordering::Relaxed) + 1;
    let mut payload_with_seqnum = Vec::with_capacity(payload.len() + 32);
    payload_with_seqnum.extend_from_slice(payload);
    payload_with_seqnum.extend_from_slice(format!("SEQNUM={seqnum}\0").as_bytes());

    let credentials = crate::SenderCredentials::kernel();
    port::publish(
        NETLINK_KOBJECT_UEVENT,
        group,
        0,
        &payload_with_seqnum,
        Some(&credentials),
        None,
        None,
    );
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        // Taking the queue lock orders release against a sender's final
        // capacity check: either the push completed before release, or the
        // blocked sender observes `closed` and retries the port lookup.
        let queue = self.inner.rx_queue.lock();
        self.inner.closed.store(true, Ordering::Release);
        drop(queue);
        // Linux removes the port before waking waiters, so a waiting sender
        // cannot pin the old `(protocol, pid)` or deliver after release.
        let pid = self.inner.local_addr.read().map(|addr| addr.pid);
        if let Some(pid) = pid {
            port::unregister(&self.inner, pid);
        }
        self.inner.poll_tx.wake();
    }
}

impl Configurable for NetlinkSocket {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        if let GetSocketOption::SendBuffer(size) = opt {
            **size = self.inner.send_buffer.load(Ordering::Relaxed);
            return Ok(OptionHandled::Yes);
        }
        if let GetSocketOption::ReceiveBuffer(size) = opt {
            **size = self.inner.rx_queue.lock().limit();
            return Ok(OptionHandled::Yes);
        }
        if let GetSocketOption::NetlinkCapAck(enabled) = opt {
            **enabled = self.inner.cap_ack.load(Ordering::Relaxed);
            return Ok(OptionHandled::Yes);
        }
        if let GetSocketOption::NetlinkMemberships(groups) = opt {
            // `netlink_getsockopt(NETLINK_LIST_MEMBERSHIPS)` copies the
            // socket's `nlk->groups` bitmap, one bit per zero-based group.
            **groups = self.inner.groups.load(Ordering::Relaxed);
            return Ok(OptionHandled::Yes);
        }
        if let GetSocketOption::Error(error) = opt {
            // `SO_ERROR` returns the pending positive errno and clears it.
            // Clearing this field does not clear receive-queue congestion;
            // Linux only clears `NETLINK_S_CONGESTED` after the queue drains.
            **error = self.inner.take_socket_error();
            return Ok(OptionHandled::Yes);
        }
        self.inner.general.get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        // `SO_SNDBUF`/`SO_RCVBUF` and their `CAP_NET_ADMIN` force forms reach
        // the same `sock_setsockopt` arms on Linux and write `sk_sndbuf` and
        // `sk_rcvbuf`, which netlink's send admission and receive queue account
        // against. The force forms skip only the `*mem_max` clamp, so they must
        // update these queues too: answering success while leaving the queue at
        // its default would contradict both the readback and the admission
        // limit imposed by the same socket layer.
        let send = match opt {
            SetSocketOption::SendBuffer(size) => Some((*size, false)),
            SetSocketOption::SendBufferForce(size) => Some((*size, true)),
            _ => None,
        };
        if let Some((size, is_force)) = send {
            // Linux clamps the unforced hint to `net.core.wmem_max`, doubles
            // it, and applies `SOCK_MIN_SNDBUF` to the effective buffer.
            let requested = if is_force {
                size
            } else {
                size.min(super::NETLINK_WMEM_MAX)
            };
            let effective = requested.saturating_mul(2).max(super::NETLINK_SNDBUF_MIN);
            self.inner.send_buffer.store(effective, Ordering::Relaxed);
            return Ok(OptionHandled::Yes);
        }
        let receive = match opt {
            SetSocketOption::ReceiveBuffer(size) => Some((*size, false)),
            SetSocketOption::ReceiveBufferForce(size) => Some((*size, true)),
            _ => None,
        };
        if let Some((size, is_force)) = receive {
            // `sock_setsockopt(SO_RCVBUF)` clamps the unforced hint to
            // `net.core.rmem_max`, doubles it, and applies `SOCK_MIN_RCVBUF`
            // to the stored `sk_rcvbuf`.
            let requested = if is_force {
                size
            } else {
                size.min(super::NETLINK_RMEM_MAX)
            };
            let effective = requested.saturating_mul(2).max(super::NETLINK_RCVBUF_MIN);
            self.inner.rx_queue.lock().set_limit(effective);
            return Ok(OptionHandled::Yes);
        }
        if let SetSocketOption::NetlinkCapAck(enabled) = opt {
            self.inner.cap_ack.store(*enabled, Ordering::Relaxed);
            return Ok(OptionHandled::Yes);
        }
        if let SetSocketOption::NetlinkAddMembership(group) = opt {
            // `netlink_setsockopt` answers EINVAL for group 0 and for any group
            // at or past the protocol's `ngroups`; silently succeeding would
            // tell a subscriber it joined a group that will never deliver.
            if !(1..=NETLINK_GROUP_COUNT as u32).contains(group) {
                return Err(KError::InvalidInput);
            }
            // `netlink_setsockopt` runs the protocol's `netlink_bind` hook, and
            // `rtnetlink_bind` requires CAP_NET_ADMIN for the reverse-path
            // groups. `bind()` enforces the same rule here; subscribing by
            // `setsockopt` must not be the cheaper way in.
            if super::port::group_is_privileged(self.inner.protocol, *group)
                && !net_capable(&kprocess::current_cred(), CAP_NET_ADMIN)
            {
                return Err(KError::OperationNotPermitted);
            }
            // `netlink_setsockopt` runs under `netlink_table_grab`, the same
            // table lock `netlink_bind` takes, because `bind` replaces the whole
            // mask and the two must not interleave. `send_lock` is this
            // socket's serialisation point and is the one `bind_inner` holds
            // across that replacement.
            let _membership_guard = self.inner.send_lock.lock();
            // The first subscription links the socket into the broadcast list
            // (`netlink_update_subscriptions` -> `sk_add_bind_node` on the
            // protocol's `mc_list`), which is independent of `bind()`: a socket
            // that never binds still receives its groups' multicast.
            if self
                .inner
                .groups
                .fetch_or(1u32 << (*group - 1), Ordering::Relaxed)
                == 0
            {
                super::port::subscribe(&self.inner);
            }
            return Ok(OptionHandled::Yes);
        }
        if let SetSocketOption::NetlinkDropMembership(group) = opt {
            if !(1..=NETLINK_GROUP_COUNT as u32).contains(group) {
                return Err(KError::InvalidInput);
            }
            // Same table lock as the add path: a concurrent rebind must not
            // replace the mask between this clear and the list update.
            let _membership_guard = self.inner.send_lock.lock();
            // The last subscription leaving unlinks the socket from the
            // broadcast list, as `netlink_update_subscriptions` does when the
            // subscription count reaches zero.
            let bit = 1u32 << (*group - 1);
            if self.inner.groups.fetch_and(!bit, Ordering::Relaxed) == bit {
                super::port::unsubscribe(&self.inner);
            }
            return Ok(OptionHandled::Yes);
        }
        self.inner.general.set_option_inner(opt)
    }
}

impl SocketOps for NetlinkSocket {
    /// Returns the `SO_TYPE` reported for this socket.
    fn socket_type(&self) -> i32 {
        self.inner.socket_type.as_raw()
    }

    fn socket_family(&self) -> i32 {
        linux_raw_sys::net::AF_NETLINK as i32
    }

    /// `netlink_create` records the requested protocol in `sk_protocol`.
    fn socket_protocol(&self) -> i32 {
        self.inner.protocol
    }

    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        // Credential-free callers cannot establish administrative authority,
        // matching the send path. Userspace bind goes through
        // `Socket::bind_with_cred` so `rtnetlink_bind` can inspect the caller.
        self.bind_inner(local_addr, false)
    }

    fn connect(&self, remote_addr: SocketAddrEx, _options: ConnectOptions) -> KResult {
        // Credential-free callers cannot claim administrative authority.
        self.connect_inner(remote_addr, false)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        // Credential-free callers cannot establish authority. Kernel callers
        // performing rtnetlink mutations must use send_with_cred explicitly.
        self.send_inner(src, options, false, false, 0)
    }

    fn recv(&self, mut dst: impl Write + IoBufMut, mut options: RecvOptions<'_>) -> KResult<usize> {
        // `netlink_recvmsg` rejects MSG_OOB before waiting for a datagram.
        if options.flags.contains(crate::RecvFlags::OOB) {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        self.inner
            .general
            .recv_poller_with_nonblocking(self, options.flags.nonblocking(), || {
                let mut rx_queue = self.inner.rx_queue.lock();
                let packet = rx_queue.front().ok_or(KError::WouldBlock)?;
                let packet_len = packet.data.len();
                if let Some(from) = options.from.as_deref_mut() {
                    *from = SocketAddrEx::Netlink(packet.from);
                }
                if self.inner.general.pass_credentials()
                    && let Some(ancillary) = options.ancillary.as_deref_mut()
                {
                    ancillary.push(Arc::new(crate::KernelAncillaryData::Credentials(
                        packet
                            .credentials
                            .clone()
                            .unwrap_or_else(crate::SenderCredentials::kernel),
                    )));
                }

                let write_len = packet_len.min(dst.remaining_mut());
                if write_len < packet_len
                    && let Some(out_flags) = options.out_flags.as_deref_mut()
                {
                    *out_flags |= RecvFlags::TRUNCATE;
                }

                // Linux removes a non-peek datagram from the receive queue
                // before copying it to userspace, and always frees it even if
                // the copy fails.  Preserve that ownership rule: a userspace
                // copy error consumes the datagram rather than leaving it
                // queued for a retry.
                if options.flags.contains(RecvFlags::PEEK) {
                    dst.write_all(&packet.data[..write_len])?;
                } else {
                    let packet = rx_queue.pop_front().ok_or(KError::WouldBlock)?;
                    let is_empty = rx_queue.is_empty();
                    drop(rx_queue);
                    // Linux `netlink_rcv_wake` clears congestion only once
                    // the receive queue has actually drained.  A blocked
                    // sender and a poller waiting for output both need the
                    // wake; clearing before waking avoids a spurious block.
                    if is_empty {
                        self.inner.congested.store(false, Ordering::Release);
                    }
                    self.inner.poll_tx.wake();
                    dst.write_all(&packet.data[..write_len])?;
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

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        // Linux `netlink_getname` reports the stored destination, and 0/0 for
        // an unconnected socket; it does not fail.
        Ok(SocketAddrEx::Netlink(
            self.inner.remote_addr.read().unwrap_or_default(),
        ))
    }

    // TODO: Define shutdown semantics if a future netlink use case needs them.
    // The current implementation models datagram-style request/response and
    // event delivery, so shutdown() is not supported.
    fn shutdown(&self, _how: Shutdown) -> KResult {
        Err(KError::OperationNotSupported)
    }
}

/// Splits a netlink datagram into complete messages.
///
/// Mirrors Linux `netlink_rcv_skb`: iterate while at least one header fits,
/// stop when `nlmsg_len` is shorter than a header or longer than the remaining
/// bytes, and ignore any trailing padding without requiring it to be zero.
fn split_netlink_messages(mut datagram: &[u8]) -> Vec<&[u8]> {
    let mut messages = Vec::new();
    while datagram.len() >= NLMSG_HDR_LEN {
        let message_len =
            u32::from_ne_bytes([datagram[0], datagram[1], datagram[2], datagram[3]]) as usize;
        if message_len < NLMSG_HDR_LEN || message_len > datagram.len() {
            break;
        }
        messages.push(&datagram[..message_len]);
        let aligned_len = align(message_len).min(datagram.len());
        datagram = &datagram[aligned_len..];
    }
    messages
}

fn normalize_reply_port(mut data: Vec<u8>, requesting_port: u32) -> NetlinkPacket {
    if data.len() >= NLMSG_HDR_LEN {
        data[12..16].copy_from_slice(&requesting_port.to_ne_bytes());
    }
    NetlinkPacket {
        from: NetlinkAddr { pid: 0, groups: 0 },
        data,
        credentials: None,
    }
}

impl Pollable for NetlinkSocket {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        if !self.inner.rx_queue.lock().is_empty() {
            events |= IoEvents::IN;
        }
        // `overrun` release-stores the pending ENOBUFS; this acquire load
        // observes it and reports ERR readiness until `SO_ERROR` consumes it.
        if self.inner.socket_error.load(Ordering::Acquire) != 0 {
            events |= IoEvents::ERR;
        }
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        // Linux `datagram_poll()` calls `sock_poll_wait()` before computing
        // readiness, so error-only and already-readable waiters also register
        // for receive wakeups.  `overrun()` publishes its `EPOLLERR` wake
        // through `poll_rx`; skipping registration for a nonempty queue would
        // lose that wakeup after the queue drains.
        if events.intersects(IoEvents::IN | IoEvents::ERR) {
            context.register(&self.inner.poll_rx)?;
        }
        Ok(())
    }
}
