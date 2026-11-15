// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket option types and configuration helpers.
use alloc::{string::String, sync::Arc};

use enum_dispatch::enum_dispatch;
use kalloc_types::KBox;
use kerrno::{KError, KResult, LinuxError};
use kidentity::{PidHandle, PidNamespace};
use ktime_types::TimeSpan;

macro_rules! define_options {
    ($($(#[$meta:meta])* $name:ident($value:ty),)*) => {
        /// Operation to get a socket option.
        ///
        /// See [`Configurable::get_option`].
        pub enum GetSocketOption<'a> {
            $(
                $(#[$meta])*
                $name(&'a mut $value),
            )*
        }

        /// Operation to set a socket option.
        ///
        /// See [`Configurable::set_option`].
        #[derive(Clone, Copy)]
        pub enum SetSocketOption<'a> {
            $(
                $(#[$meta])*
                $name(&'a $value),
            )*
        }
    };
}

/// Raw `SO_PEERCRED` result in the Linux `struct ucred` layout.
///
/// This is an ABI carrier only. A socket stores a [`PeerCredentials`] snapshot
/// and resolves it at `getsockopt` time, because the peer PID must be
/// projected into the observer's active PID namespace.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct UnixCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Default for UnixCredentials {
    fn default() -> Self {
        // Linux `sock_get_peercred` reports no peer as a zero PID and `-1`
        // (as `u32`) for both IDs.
        Self {
            pid: 0,
            uid: u32::MAX,
            gid: u32::MAX,
        }
    }
}

impl UnixCredentials {
    pub fn new(pid: u32, uid: u32, gid: u32) -> Self {
        UnixCredentials { pid, uid, gid }
    }
}

/// Snapshot of the credentials established for a Unix-domain peer.
///
/// The PID remains a process identity until [`Self::resolve_in`] so each
/// observer can apply Linux's `pid_vnr` projection at `getsockopt` time.
/// `SO_PEERCRED` records effective credentials, unlike per-message
/// `SCM_CREDENTIALS`, which uses real credentials.
#[derive(Default, Debug, Clone)]
pub struct PeerCredentials {
    identity: Option<PeerIdentity>,
}

#[derive(Debug, Clone)]
struct PeerIdentity {
    pid: Arc<PidHandle>,
    uid: u32,
    gid: u32,
}

impl PeerCredentials {
    /// Creates a snapshot for a socket that has an established peer.
    pub fn new(pid: Arc<PidHandle>, uid: u32, gid: u32) -> Self {
        Self {
            identity: Some(PeerIdentity { pid, uid, gid }),
        }
    }

    /// Creates a snapshot for a socket without a peer.
    pub fn none() -> Self {
        Self::default()
    }

    /// Projects the snapshot through `namespace`, matching Linux
    /// `sock_get_peercred`.
    ///
    /// A missing peer reports `(0, -1, -1)`. A peer invisible from `namespace`
    /// keeps its UID/GID but reports PID 0, as `pid_vnr` does.
    pub fn resolve_in(&self, namespace: &Arc<PidNamespace>) -> UnixCredentials {
        match &self.identity {
            Some(identity) => UnixCredentials {
                pid: identity.pid.number_in(namespace).unwrap_or(0),
                uid: identity.uid,
                gid: identity.gid,
            },
            None => UnixCredentials::default(),
        }
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PacketStatistics {
    pub packets: u32,
    pub drops: u32,
}

#[derive(Default, Debug, Clone, Copy)]
pub struct PacketMembership {
    pub ifindex: i32,
    pub membership_type: u16,
    pub addr_len: u16,
    pub addr: [u8; 8],
}

define_options! {
    // ---- Socket-wide options ----
    ReuseAddress(bool),
    Error(i32),
    DontRoute(bool),
    Broadcast(bool),
    SendBuffer(usize),
    ReceiveBuffer(usize),
    KeepAlive(bool),
    /// TCP close policy: disabled, or a zero duration for an abortive close.
    /// Positive linger durations are not supported.
    Linger(Option<TimeSpan>),
    SendTimeout(TimeSpan),
    ReceiveTimeout(TimeSpan),
    SendBufferForce(usize),
    PassCredentials(bool),
    PassSecurity(bool),
    PassRights(bool),
    Timestamp(bool),
    TimestampNs(bool),
    Timestamping(u32),
    PeerCredentials(PeerCredentials),
    /// `NETLINK_CAP_ACK`: omit the original request payload from error
    /// acknowledgements and mark them `NLM_F_CAPPED`.
    NetlinkCapAck(bool),
    /// `NETLINK_EXT_ACK`: request extended ACK error reporting. Tracked only.
    NetlinkExtAck(bool),
    /// `NETLINK_GET_STRICT_CHK`: strict dump checking. Tracked only.
    NetlinkGetStrictChk(bool),
    /// `NETLINK_PKTINFO`: receive packet info ancillary data. Tracked only.
    NetlinkPktinfo(bool),
    /// `NETLINK_ADD_MEMBERSHIP` / `NETLINK_DROP_MEMBERSHIP`: multicast group id.
    NetlinkAddMembership(u32),
    NetlinkDropMembership(u32),
    /// `NETLINK_LIST_MEMBERSHIPS`: the socket's bound multicast group mask,
    /// zero-based bit per group (`netlink_getsockopt` copies `nlk->groups`).
    NetlinkMemberships(u32),
    /// `IP_PKTINFO`: receive packet-info ancillary data on UDP. Tracked only.
    IpPktinfo(bool),
    /// `IP_RECVTTL` / `IP_RECVTOS`: receive TTL/TOS ancillary data. Tracked only.
    IpRecvTtl(bool),
    IpRecvTos(bool),
    /// `SO_BINDTOIFINDEX`: bind to a device by ifindex (like SO_BINDTODEVICE).
    BindToIfindex(i32),
    /// `TCP_FASTOPEN`: fast-open queue length. Tracked only.
    TcpFastopen(u32),
    /// `SO_ACCEPTCONN`: whether the socket is listening (read-only).
    AcceptConn(bool),
    /// `SO_RCVBUFFORCE`: privileged receive-buffer override.
    ReceiveBufferForce(usize),
    /// `IP_MULTICAST_TTL`: multicast TTL. Tracked only.
    IpMulticastTtl(u32),
    /// `IP_RECVFRAGSIZE`: receive fragment-size ancillary data. Tracked only.
    IpRecvFragsize(bool),
    /// `SO_PASSPIDFD`: pass peer pidfd ancillary data. Tracked only.
    PassPidfd(bool),

    // ---- TCP options ----
    NoDelay(bool),
    MaxSegment(usize),
    TcpInfo(()),
    /// Seconds an idle connection waits before the first keep-alive probe.
    ///
    /// This is applied to smoltcp's single keep-alive interval. Once keep-alive
    /// has been enabled, updates are rejected with `EOPNOTSUPP`.
    KeepIdle(u32),
    /// Seconds between keep-alive probes.
    ///
    /// Recorded and reported for Linux ABI compatibility. smoltcp has no
    /// independent probe interval, so this does not alter transport timing.
    /// This is an accepted ABI divergence: see `KeepAliveConfig`.
    KeepInterval(u32),
    /// Failed probes after which the connection is dropped.
    ///
    /// Recorded and reported for Linux ABI compatibility. smoltcp has no
    /// probe-count control, so this does not alter transport timing. This is
    /// an accepted ABI divergence: see `KeepAliveConfig`.
    KeepCount(u32),

    // ---- IP options ----
    Ttl(u8),
    RecvErr(bool),
    MtuDiscover(u8),

    // ---- Packet socket options (PACKET_*) ----
    PacketStatistics(PacketStatistics),
    PacketAddMembership(PacketMembership),
    PacketDropMembership(PacketMembership),

    // ---- Extra options ----
    NonBlocking(bool),
    BindToDevice(Option<String>),
}

/// Whether a socket option is handled by a specific socket implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionHandled {
    Yes,
    No,
}

impl OptionHandled {
    pub fn is_yes(self) -> bool {
        self == Self::Yes
    }
}

/// Trait for configurable socket-like objects.
#[enum_dispatch]
pub trait Configurable {
    /// Get a socket option if the socket supports it.
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled>;
    /// Set a socket option if the socket supports it.
    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled>;

    fn get_option(&self, mut opt: GetSocketOption) -> KResult {
        match self.get_option_inner(&mut opt)? {
            OptionHandled::Yes => Ok(()),
            OptionHandled::No => Err(KError::from(LinuxError::ENOPROTOOPT)),
        }
    }
    fn set_option(&self, opt: SetSocketOption) -> KResult {
        match self.set_option_inner(opt)? {
            OptionHandled::Yes => Ok(()),
            OptionHandled::No => Err(KError::from(LinuxError::ENOPROTOOPT)),
        }
    }
}

impl<T: Configurable + ?Sized> Configurable for KBox<T> {
    fn get_option_inner(&self, opt: &mut GetSocketOption) -> KResult<OptionHandled> {
        (**self).get_option_inner(opt)
    }

    fn set_option_inner(&self, opt: SetSocketOption) -> KResult<OptionHandled> {
        (**self).set_option_inner(opt)
    }
}
