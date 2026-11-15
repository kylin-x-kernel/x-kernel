// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Netlink port ID allocation and multicast delivery.
//!
//! A netlink socket becomes addressable when it binds.  The port ID is unique
//! per protocol.  Linux allows a bound socket to change only its subscription
//! mask: every rebind must repeat the current port ID, while a first bind with
//! `nl_pid == 0` allocates one.  Keeping the registry here, rather than in a
//! protocol-specific subscriber list, gives both kernel notifications and
//! userspace multicast the same addressing and filtering rules.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::Ordering;

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use kprocess::AsThread;
use ksync::{Mutex, static_lock};
use ktask::future::{block_on, poll_io};

use super::{
    NETLINK_KOBJECT_UEVENT, NETLINK_ROUTE, NETLINK_USERSOCK, NetlinkAddr, NetlinkPacket,
    NetlinkSocketInner, RTNLGRP_IPV4_MROUTE_R, RTNLGRP_IPV6_MROUTE_R,
};
use crate::SenderCredentials;

static_lock! {
    static PORTS: Mutex<PortTable> = Mutex::new(PortTable::new());
}

struct PortTable {
    sockets: BTreeMap<(i32, u32), Weak<NetlinkSocketInner>>,
    /// Sockets holding at least one group subscription, the equivalent of
    /// Linux's per-protocol `mc_list` (`netlink_update_subscriptions` ->
    /// `sk_add_bind_node`).
    ///
    /// This list is what makes an *unbound* subscriber reachable: the port map
    /// above is keyed by the id `bind()` allocates, and `NETLINK_ADD_MEMBERSHIP`
    /// is valid without one.
    subscribers: Vec<Weak<NetlinkSocketInner>>,
}

impl PortTable {
    const fn new() -> Self {
        Self {
            sockets: BTreeMap::new(),
            subscribers: Vec::new(),
        }
    }

    fn retain_live(&mut self) {
        let is_live = |socket: &Weak<NetlinkSocketInner>| {
            socket
                .upgrade()
                .is_some_and(|socket| !socket.closed.load(Ordering::Acquire))
        };
        self.sockets.retain(|_, socket| is_live(socket));
        self.subscribers.retain(is_live);
    }

    /// Allocates an unused port ID for an unbound socket.
    ///
    /// A userspace autobind starts with the caller's process ID, as
    /// `task_tgid_vnr(current)` does.  If that ID is already in use, Linux
    /// searches the reserved negative range down to `INT_MIN`.  The random
    /// starting point of Linux's probe is not part of the address semantics,
    /// so this scan is deterministic and starts at the top of that range.
    fn allocate(&mut self, protocol: i32, preferred: Option<u32>) -> Option<u32> {
        if let Some(pid) = preferred
            && pid != 0
            && !self.sockets.contains_key(&(protocol, pid))
        {
            return Some(pid);
        }

        (0x8000_0000..=0xffff_efff)
            .rev()
            .find(|pid| !self.sockets.contains_key(&(protocol, *pid)))
    }

    fn claim(
        &mut self,
        protocol: i32,
        pid: u32,
        socket: &Arc<NetlinkSocketInner>,
    ) -> Result<(), LinuxError> {
        let key = (protocol, pid);
        match self.sockets.get(&key).and_then(Weak::upgrade) {
            Some(existing) if Arc::ptr_eq(&existing, socket) => Ok(()),
            // A released socket may still have a strong reference held by a
            // blocked sender.  It no longer owns its port, so a new bind may
            // replace the entry as soon as the receiver is closed.
            Some(existing) if existing.closed.load(Ordering::Acquire) => {
                self.sockets.insert(key, Arc::downgrade(socket));
                Ok(())
            }
            Some(_) => Err(LinuxError::EADDRINUSE),
            // The last strong reference may disappear between `retain_live`
            // and this lookup.  A dead weak entry does not own the port, so
            // replace it instead of reporting a spurious collision.
            None => {
                self.sockets.insert(key, Arc::downgrade(socket));
                Ok(())
            }
        }
    }
}

/// Multicast policy attached to a registered netlink protocol.
#[derive(Clone, Copy)]
struct GroupPolicy {
    /// Group bits the protocol implements.  `netlink_bind` masks subscriptions
    /// to this value.
    mask: u32,
    /// Whether `NL_CFG_F_NONROOT_RECV` lets an unprivileged socket subscribe.
    allows_unprivileged_receive: bool,
    /// Whether `NL_CFG_F_NONROOT_SEND` lets an unprivileged socket send to a
    /// nonzero port or group.
    allows_unprivileged_send: bool,
    /// Group bits that a protocol bind callback additionally restricts to
    /// `CAP_NET_ADMIN`.
    privileged_groups: u32,
}

fn group_policy(protocol: i32) -> Option<GroupPolicy> {
    match protocol {
        NETLINK_ROUTE => Some(GroupPolicy {
            mask: u32::MAX,
            allows_unprivileged_receive: true,
            allows_unprivileged_send: false,
            privileged_groups: group_bit(RTNLGRP_IPV4_MROUTE_R) | group_bit(RTNLGRP_IPV6_MROUTE_R),
        }),
        NETLINK_USERSOCK => Some(GroupPolicy {
            mask: u32::MAX,
            allows_unprivileged_receive: false,
            allows_unprivileged_send: true,
            privileged_groups: 0,
        }),
        NETLINK_KOBJECT_UEVENT => Some(GroupPolicy {
            // `__netlink_kernel_create` raises every protocol's group table to
            // at least 32 entries, even though uevent publishes only group 1.
            mask: u32::MAX,
            allows_unprivileged_receive: true,
            allows_unprivileged_send: false,
            privileged_groups: 0,
        }),
        _ => None,
    }
}

pub(super) const fn group_bit(group: u32) -> u32 {
    1u32 << (group - 1)
}

/// Returns the process ID Linux uses as the first autobind candidate.
///
/// Kernel tasks have no process-visible ID; their sockets fall back to the
/// reserved negative range rather than borrowing a userspace process ID.
///
/// # Execution context
///
/// This must run in a user-visible thread context, never in IRQ or NMI
/// context.  The lookup does not sleep and callers may hold `send_lock`; it
/// must remain free of operations that can block.
fn current_autobind_pid() -> Option<u32> {
    ktask::current()
        .try_as_thread()
        .map(|thread| thread.process().visible_pid())
        .filter(|pid| *pid != 0)
}

/// Removes `socket` from the port table if it still owns `pid`.
///
/// A receiver's port must disappear before it wakes blocked senders, as Linux
/// `netlink_release` removes the socket from the port table before
/// `wake_up_interruptible_all`.  The identity check keeps a concurrent rebind
/// of the same numeric port intact.
pub(super) fn unregister(socket: &Arc<NetlinkSocketInner>, pid: u32) {
    let mut ports = PORTS.lock();
    let key = (socket.protocol, pid);
    if ports
        .sockets
        .get(&key)
        .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(socket)))
    {
        ports.sockets.remove(&key);
    }
}

/// Returns whether an unprivileged socket may send to a nonzero port ID or
/// group mask.
///
/// Linux gates that decision on `NL_CFG_F_NONROOT_SEND` in the protocol table;
/// only `NETLINK_USERSOCK` sets it.  `NETLINK_ROUTE` deliberately does not, so
/// route requests and multicast from unprivileged sockets still require
/// `CAP_NET_ADMIN`.
pub(super) fn allows_unprivileged_send(protocol: i32) -> bool {
    group_policy(protocol).is_some_and(|policy| policy.allows_unprivileged_send)
}

/// Whether `group` is one `rtnetlink_bind` restricts to CAP_NET_ADMIN.
///
/// The reverse-path groups expose routing topology, so Linux refuses them to an
/// unprivileged subscriber through both `bind()` and `NETLINK_ADD_MEMBERSHIP`.
pub(super) fn group_is_privileged(protocol: i32, group: u32) -> bool {
    group_policy(protocol).is_some_and(|policy| policy.privileged_groups & group_bit(group) != 0)
}

/// Returns whether an unprivileged socket may join a multicast group.
///
/// Linux gates this on `NL_CFG_F_NONROOT_RECV`. `NETLINK_ROUTE` and
/// `NETLINK_KOBJECT_UEVENT` set it; `NETLINK_USERSOCK` sets only
/// `NL_CFG_F_NONROOT_SEND`, so its groups need `CAP_NET_ADMIN`.
pub(super) fn allows_unprivileged_receive(protocol: i32) -> bool {
    group_policy(protocol).is_some_and(|policy| policy.allows_unprivileged_receive)
}

/// Binds `socket` to `requested`, allocating a port ID when an unbound socket
/// leaves `requested.pid` zero.
///
/// Linux rejects every rebind that does not repeat the current port ID with
/// `EINVAL`, including a zero port ID.  Group bits the protocol does not
/// implement are masked off rather than rejected, matching `netlink_bind`.
pub(super) fn bind(
    socket: &Arc<NetlinkSocketInner>,
    requested: NetlinkAddr,
    current: Option<NetlinkAddr>,
    is_privileged: bool,
) -> Result<NetlinkAddr, LinuxError> {
    // `netlink_bind` first requires privilege for a nonzero group request and
    // looks up the protocol's group table.  Only registered protocols can
    // allocate a group table; an unknown protocol therefore fails before the
    // port-ID conformance check below.
    let policy = if requested.groups == 0 {
        None
    } else {
        let Some(policy) = group_policy(socket.protocol) else {
            return Err(if is_privileged {
                LinuxError::ENOENT
            } else {
                LinuxError::EPERM
            });
        };
        if !policy.allows_unprivileged_receive && !is_privileged {
            return Err(LinuxError::EPERM);
        }
        Some(policy)
    };

    if current.is_some_and(|current| requested.pid != current.pid) {
        return Err(LinuxError::EINVAL);
    }

    // `rtnetlink_bind` runs after the generic port checks and can veto
    // individual groups, at which point Linux has not inserted the socket.
    if let Some(policy) = policy
        && !is_privileged
        && requested.groups & policy.privileged_groups != 0
    {
        return Err(LinuxError::EPERM);
    }

    let autobind_pid = if current.is_none() && requested.pid == 0 {
        current_autobind_pid()
    } else {
        None
    };

    let mut ports = PORTS.lock();
    ports.retain_live();

    let pid = match current {
        Some(current) => current.pid,
        None if requested.pid != 0 => requested.pid,
        None => ports
            .allocate(socket.protocol, autobind_pid)
            .ok_or(LinuxError::EADDRINUSE)?,
    };

    ports.claim(socket.protocol, pid, socket)?;
    Ok(NetlinkAddr {
        pid,
        groups: requested.groups & policy.map_or(0, |policy| policy.mask),
    })
}

/// Links a socket into the protocol's broadcast list.
///
/// Called when `NETLINK_ADD_MEMBERSHIP` takes the socket from no subscription
/// to one, mirroring `netlink_update_subscriptions`' `sk_add_bind_node`. An
/// unbound socket lives only here, which is why `publish` walks both this list
/// and the port map.
pub(super) fn subscribe(socket: &Arc<NetlinkSocketInner>) {
    let mut ports = PORTS.lock();
    ports.retain_live();
    let ptr = Arc::as_ptr(socket);
    if ports
        .subscribers
        .iter()
        .filter_map(Weak::upgrade)
        .any(|existing| Arc::as_ptr(&existing) == ptr)
    {
        return;
    }
    ports.subscribers.push(Arc::downgrade(socket));
}

/// Unlinks a socket whose last subscription was dropped.
pub(super) fn unsubscribe(socket: &Arc<NetlinkSocketInner>) {
    let mut ports = PORTS.lock();
    let ptr = Arc::as_ptr(socket);
    ports.subscribers.retain(|weak| match weak.upgrade() {
        Some(existing) => Arc::as_ptr(&existing) != ptr,
        None => false,
    });
}

/// Delivers `data` to every live socket of `protocol` whose subscription mask
/// contains `group`: bound sockets from the port map plus unbound subscribers
/// from the broadcast list.
///
/// Delivery is best effort: a full receive queue drops the packet without
/// blocking the publisher, matching kernel netlink multicast.  Userspace
/// multicast excludes both the sending socket and the requested unicast
/// destination, as Linux `do_one_broadcast` does.  Kernel notifications exclude
/// neither.
///
/// Callers must run in sleepable task context.  The registry and each
/// receiver's receive queue use blocking mutexes, so this must never be called
/// from an IRQ or softirq handler.
pub(super) fn publish(
    protocol: i32,
    group: u32,
    source_pid: u32,
    data: &[u8],
    credentials: Option<&SenderCredentials>,
    exclude_socket: Option<&Arc<NetlinkSocketInner>>,
    exclude_pid: Option<u32>,
) {
    if group == 0 || group > u32::BITS {
        return;
    }
    let mask = 1u32 << (group - 1);
    // Snapshot recipients under the registry lock, but enqueue and wake only
    // after dropping it: a full queue must not extend the critical section, and
    // the wake-up path must never be able to re-enter the port table.
    let recipients: Vec<_> = {
        let mut ports = PORTS.lock();
        ports.retain_live();
        // A subscribed socket is in both tables once it binds, so the union is
        // de-duplicated by identity; delivering twice would duplicate messages
        // a bound subscriber expects once.
        let mut seen = BTreeSet::new();
        ports
            .sockets
            .values()
            .chain(ports.subscribers.iter())
            .filter_map(Weak::upgrade)
            .filter(|socket| socket.protocol == protocol)
            .filter(|socket| !exclude_socket.is_some_and(|exclude| Arc::ptr_eq(exclude, socket)))
            .filter(|socket| seen.insert(Arc::as_ptr(socket) as usize))
            .collect()
    };

    for socket in recipients {
        // `netlink_setsockopt(NETLINK_ADD_MEMBERSHIP)` links the socket into the
        // protocol's `mc_list` (`netlink_update_subscriptions` ->
        // `sk_add_bind_node`), and `netlink_broadcast_filtered` walks exactly
        // that list with `sk_for_each_bound`. An unbound subscriber is therefore
        // a recipient; only the sender-exclusion check below needs a port id.
        //
        // The registry snapshot above is not the subscription decision, so the
        // live mask is read here, once per recipient. The mask is an atomic the
        // subscribe/unsubscribe path updates without this record's locks, so a
        // concurrent unsubscribe is observed either before this read (the packet
        // is suppressed) or after it (the packet is already claimed and is
        // queued) - never lost between a check and a push.
        let addr_guard = socket.local_addr.read();
        let local_addr = *addr_guard;
        if socket.groups.load(Ordering::Relaxed) & mask == 0 {
            continue;
        }
        if local_addr.is_some_and(|addr| exclude_pid == Some(addr.pid)) {
            continue;
        }
        let packet = NetlinkPacket {
            from: NetlinkAddr {
                pid: source_pid,
                groups: mask,
            },
            data: data.to_vec(),
            credentials: credentials.cloned(),
        };
        let mut queue = socket.rx_queue.lock();
        // `Drop` takes this lock before marking the socket closed, so a
        // packet can never be inserted after the receiver starts releasing.
        if socket.closed.load(Ordering::Acquire) {
            continue;
        }
        // Multicast is best effort, but every failed delivery to an eligible
        // subscribed receiver reports an overrun.
        let delivered = !socket.is_congested() && queue.push_back(packet);
        drop(queue);
        drop(addr_guard);
        if delivered {
            socket.poll_rx.wake();
        } else {
            socket.overrun();
        }
    }
}

/// Readiness adapter for a unicast send waiting for receive-queue capacity.
struct NetlinkQueueSpace<'a> {
    socket: &'a NetlinkSocketInner,
    /// Byte length of the datagram being admitted.  `can_push()` accounts
    /// `sk_rmem_alloc` in bytes, so the poll wake must use the same unit.
    len_bytes: usize,
}

impl NetlinkQueueSpace<'_> {
    fn has_space(&self) -> bool {
        !self.socket.closed.load(Ordering::Acquire)
            && !self.socket.is_congested()
            && self.socket.rx_queue.lock().can_push(self.len_bytes)
    }
}

impl Pollable for NetlinkQueueSpace<'_> {
    fn poll(&self) -> IoEvents {
        if self.has_space() {
            IoEvents::OUT
        } else {
            IoEvents::empty()
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.contains(IoEvents::OUT) {
            return Ok(());
        }
        // Hold the queue lock across registration.  A receiver that pops an
        // entry must take this lock before waking `poll_tx`, so it cannot
        // publish a wake-up between the capacity check and registration.
        let queue = self.socket.rx_queue.lock();
        if self.socket.closed.load(Ordering::Acquire) {
            // The receiver may have released after the operation observed a
            // full queue but before this registration.  Its wake-up has
            // already been published, so force one recheck instead of
            // sleeping without a wake-up source.
            drop(queue);
            context.wake_by_ref();
        } else if self.socket.is_congested() || !queue.can_push(self.len_bytes) {
            context.register(&self.socket.poll_tx)?;
        }
        Ok(())
    }
}

/// Delivers one packet to the socket bound to `(protocol, pid)`.
///
/// A missing port is `ECONNREFUSED`, matching `netlink_getsockbyportid`.
/// Linux unicast is reliable: when the destination queue is full, a blocking
/// send waits for capacity and a nonblocking send fails with `EAGAIN`.  A
/// receiver that releases while the sender waits is removed from the port
/// table before the wake-up, so the sender retries the port lookup: it either
/// fails with `ECONNREFUSED` or delivers to a socket that has reused the port.
/// Multicast remains best effort and drops on a full queue.
pub(super) fn deliver_unicast(
    protocol: i32,
    pid: u32,
    source_pid: u32,
    source_groups: u32,
    data: &[u8],
    credentials: Option<&SenderCredentials>,
    nonblocking: bool,
) -> Result<(), LinuxError> {
    let from = NetlinkAddr {
        pid: source_pid,
        groups: source_groups,
    };
    loop {
        let socket = {
            let mut ports = PORTS.lock();
            ports.retain_live();
            ports.sockets.get(&(protocol, pid)).and_then(Weak::upgrade)
        }
        .ok_or(LinuxError::ECONNREFUSED)?;

        // A registry snapshot can race with release before the entry is
        // removed.  Retry the lookup rather than reporting a stale port.
        if socket.closed.load(Ordering::Acquire) {
            continue;
        }
        // This mirrors `netlink_getsockbyportid`: a connected destination only
        // accepts unicast from the peer it connected to.  Broadcast is not
        // subject to this check.
        if socket
            .remote_addr
            .read()
            .is_some_and(|remote| remote.pid != source_pid)
        {
            return Err(LinuxError::ECONNREFUSED);
        }

        let space = NetlinkQueueSpace {
            socket: &socket,
            len_bytes: data.len(),
        };
        let result = block_on(poll_io(
            &space,
            IoEvents::OUT,
            nonblocking,
            || -> KResult<bool> {
                let mut queue = socket.rx_queue.lock();
                if socket.closed.load(Ordering::Acquire) {
                    return Ok(false);
                }
                if socket.is_congested()
                    || !queue.push_back(NetlinkPacket {
                        from,
                        data: data.to_vec(),
                        credentials: credentials.cloned(),
                    })
                {
                    return Err(KError::WouldBlock);
                }
                drop(queue);
                socket.poll_rx.wake();
                Ok(true)
            },
        ));
        let delivered = match result {
            Ok(delivered) => delivered,
            Err(KError::WouldBlock) => {
                // A nonblocking delivery originated by the kernel reports the
                // overrun to the receiver.  User-to-user unicast just returns
                // EAGAIN, matching `netlink_attachskb`.
                if nonblocking && source_pid == 0 {
                    socket.overrun();
                    return Err(LinuxError::EAGAIN);
                }
                return Err(KError::WouldBlock.into());
            }
            Err(error) => return Err(error.into()),
        };
        if delivered {
            return Ok(());
        }
    }
}
