// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket option syscalls.
//!
//! This module implements socket option manipulation including:
//! - Get socket options (getsockopt, etc.)
//! - Set socket options (setsockopt, etc.)
//! - Socket-level, IP-level, TCP-level, and other protocol options
//!
//! Linux stores nonzero send and receive timeouts in jiffies. Socket timeout
//! conversion therefore rounds a userspace `timeval` up to one kernel tick.

use alloc::{sync::Arc, vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use knet::{
    Socket, SocketOps,
    options::{Configurable, GetSocketOption, PeerCredentials, SetSocketOption, UnixCredentials},
    sock_from_file,
};
use linux_raw_sys::net::{
    SO_ATTACH_FILTER, SO_BINDTODEVICE, SO_DETACH_FILTER, SO_DOMAIN, SO_PEERCRED, SO_PROTOCOL,
    SO_TYPE, SOL_NETLINK, SOL_SOCKET, socklen_t,
};
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr, UserRead};

use crate::legacy_iptables::{LegacyGetOption, LegacySetOption};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;

/// `NETLINK_CAP_ACK` is not exposed by `linux_raw_sys` on every target.
const NETLINK_CAP_ACK: u32 = 10;
/// Netlink options not exposed by `linux_raw_sys` on every target.
const NETLINK_ADD_MEMBERSHIP: u32 = 1;
const NETLINK_DROP_MEMBERSHIP: u32 = 2;
const NETLINK_PKTINFO: u32 = 3;
const NETLINK_LIST_MEMBERSHIPS: u32 = 9;
const NETLINK_EXT_ACK: u32 = 11;
const NETLINK_GET_STRICT_CHK: u32 = 12;

const PACKET_ADD_MEMBERSHIP: u32 = 1;
const PACKET_DROP_MEMBERSHIP: u32 = 2;
const PACKET_STATISTICS: u32 = 6;

const IFNAMSIZ: usize = 16;
const BPF_MAX_INSNS: usize = 4096;
const BPF_CLASS_MASK: u16 = 0x07;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_OP_MASK: u16 = 0xf0;
const BPF_JA: u16 = 0x00;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

// SAFETY: `SockFilter` is the Linux classic-BPF instruction ABI and contains
// only plain integer fields with no invalid bit patterns.
unsafe impl UserRead for SockFilter {}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFprog {
    len: u16,
    filter: usize,
}

// SAFETY: `SockFprog` is copied only as an ABI carrier. The embedded pointer
// is validated and copied through `UserConstPtr` before any instruction use.
unsafe impl UserRead for SockFprog {}

fn validate_classic_filter(program: SockFprog) -> KResult<()> {
    let len = usize::from(program.len);
    if len == 0 || len > BPF_MAX_INSNS || program.filter == 0 {
        return Err(KError::InvalidInput);
    }
    let instructions = UserConstPtr::<SockFilter>::from(program.filter).load_vm_vec(len)?;
    validate_classic_filter_instructions(&instructions)
}

fn validate_classic_filter_instructions(instructions: &[SockFilter]) -> KResult<()> {
    let len = instructions.len();
    if instructions
        .last()
        .is_none_or(|instruction| instruction.code & BPF_CLASS_MASK != BPF_RET)
    {
        return Err(KError::InvalidInput);
    }
    for (index, instruction) in instructions.iter().enumerate() {
        if instruction.code & BPF_CLASS_MASK != BPF_JMP {
            continue;
        }
        let remaining = len - index - 1;
        if instruction.code & BPF_OP_MASK == BPF_JA {
            if instruction.k as usize >= remaining {
                return Err(KError::InvalidInput);
            }
        } else if usize::from(instruction.jt) >= remaining
            || usize::from(instruction.jf) >= remaining
        {
            return Err(KError::InvalidInput);
        }
    }
    Ok(())
}

fn supports_kobject_filter_compat(socket: &knet::Socket) -> bool {
    matches!(socket, knet::Socket::Netlink(netlink) if netlink.is_kobject_uevent())
}

mod conv {
    use kerrno::{KError, KResult};
    use knet::options::{PacketMembership, PacketStatistics};
    use ktime_types::{NANOS_PER_SEC, TimeSpan};
    use linux_raw_sys::general::timeval;
    use posix_types::{TimeSpanLike, UserRead};

    pub(super) struct Int<T>(T);

    impl<T: TryFrom<i32> + TryInto<i32>> Int<T> {
        pub(super) fn sys_to_rust(val: i32) -> KResult<T> {
            T::try_from(val).map_err(|_| KError::InvalidInput)
        }

        pub(super) fn rust_to_sys(val: T) -> KResult<i32> {
            val.try_into().map_err(|_| KError::InvalidInput)
        }
    }

    pub(super) struct IntBool;

    impl IntBool {
        pub(super) fn sys_to_rust(val: i32) -> KResult<bool> {
            Ok(val != 0)
        }

        pub(super) fn rust_to_sys(val: bool) -> KResult<i32> {
            Ok(val as _)
        }
    }

    pub(super) struct SocketTimeout;

    pub(super) struct SocketLinger;

    impl SocketLinger {
        pub(super) fn sys_to_rust(val: [i32; 2]) -> KResult<Option<TimeSpan>> {
            match val {
                [0, _] => Ok(None),
                [_, 0] => Ok(Some(TimeSpan::ZERO)),
                _ => Err(KError::OperationNotSupported),
            }
        }

        pub(super) fn rust_to_sys(val: Option<TimeSpan>) -> KResult<[i32; 2]> {
            Ok([i32::from(val.is_some()), 0])
        }
    }

    impl SocketTimeout {
        pub(super) fn sys_to_rust(val: timeval) -> KResult<TimeSpan> {
            let timeout = val.try_into_time_span()?;
            if timeout.is_zero() {
                return Ok(timeout);
            }

            let nanos_per_tick = NANOS_PER_SEC as u128 / kbuild_config::TICKS_PER_SECOND as u128;
            let ticks = timeout.as_nanos().div_ceil(nanos_per_tick);
            // Tick rounding must not turn a valid userspace timeout into a
            // conversion error when it crosses the internal duration range.
            Ok(
                TimeSpan::try_from_nanos(ticks.saturating_mul(nanos_per_tick))
                    .unwrap_or(TimeSpan::MAX),
            )
        }

        pub(super) fn rust_to_sys(val: TimeSpan) -> KResult<timeval> {
            Ok(timeval::from_time_span(val))
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct tpacket_stats {
        pub tp_packets: u32,
        pub tp_drops: u32,
    }

    // SAFETY: `tpacket_stats` is a POD getsockopt result structure.
    unsafe impl UserRead for tpacket_stats {}

    pub(super) struct PacketStats;

    impl PacketStats {
        pub(super) fn sys_to_rust(val: tpacket_stats) -> KResult<PacketStatistics> {
            Ok(PacketStatistics {
                packets: val.tp_packets,
                drops: val.tp_drops,
            })
        }

        pub(super) fn rust_to_sys(val: PacketStatistics) -> KResult<tpacket_stats> {
            Ok(tpacket_stats {
                tp_packets: val.packets,
                tp_drops: val.drops,
            })
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct packet_mreq {
        pub mr_ifindex: i32,
        pub mr_type: u16,
        pub mr_alen: u16,
        pub mr_address: [u8; 8],
    }

    // SAFETY: `packet_mreq` is a POD setsockopt/getsockopt carrier.
    unsafe impl UserRead for packet_mreq {}

    pub(super) struct PacketMembershipConv;

    impl PacketMembershipConv {
        pub(super) fn sys_to_rust(val: packet_mreq) -> KResult<PacketMembership> {
            Ok(PacketMembership {
                ifindex: val.mr_ifindex,
                membership_type: val.mr_type,
                addr_len: val.mr_alen,
                addr: val.mr_address,
            })
        }

        pub(super) fn rust_to_sys(val: PacketMembership) -> KResult<packet_mreq> {
            Ok(packet_mreq {
                mr_ifindex: val.ifindex,
                mr_type: val.membership_type,
                mr_alen: val.addr_len,
                mr_address: val.addr,
            })
        }
    }
}

macro_rules! call_dispatch {
    ($dispatch:ident, $pat:expr) => {{
        use conv::*;
        use linux_raw_sys::net::*;

        call_dispatch! {
            $dispatch, $pat,
            (SOL_SOCKET, SO_REUSEADDR) => ReuseAddress as IntBool,
            (SOL_SOCKET, SO_ERROR) => Error,
            (SOL_SOCKET, SO_DONTROUTE) => DontRoute as IntBool,
            (SOL_SOCKET, SO_BROADCAST) => Broadcast as IntBool,
            (SOL_SOCKET, SO_SNDBUF) => SendBuffer as Int<usize>,
            (SOL_SOCKET, SO_RCVBUF) => ReceiveBuffer as Int<usize>,
            (SOL_SOCKET, SO_SNDBUFFORCE) => SendBufferForce as Int<usize>,
            (SOL_SOCKET, SO_KEEPALIVE) => KeepAlive as IntBool,
            (SOL_SOCKET, SO_LINGER) => Linger as SocketLinger,
            (SOL_SOCKET, SO_RCVTIMEO) => ReceiveTimeout as SocketTimeout,
            (SOL_SOCKET, SO_SNDTIMEO) => SendTimeout as SocketTimeout,
            (SOL_SOCKET, SO_PASSCRED) => PassCredentials as IntBool,
            (SOL_SOCKET, SO_PASSSEC) => PassSecurity as IntBool,
            (SOL_SOCKET, SO_PASSRIGHTS) => PassRights as IntBool,
            (SOL_SOCKET, SO_ACCEPTCONN) => AcceptConn as IntBool,
            (SOL_SOCKET, SO_RCVBUFFORCE) => ReceiveBufferForce as Int<usize>,
            (SOL_SOCKET, SO_BINDTOIFINDEX) => BindToIfindex as Int<i32>,
            (SOL_SOCKET, SO_PASSPIDFD) => PassPidfd as IntBool,
            (SOL_SOCKET, SO_TIMESTAMP) => Timestamp as IntBool,
            (SOL_SOCKET, SO_TIMESTAMP_NEW) => Timestamp as IntBool,
            (SOL_SOCKET, SO_TIMESTAMPNS) => TimestampNs as IntBool,
            (SOL_SOCKET, SO_TIMESTAMPNS_NEW) => TimestampNs as IntBool,
            (SOL_SOCKET, SO_TIMESTAMPING) => Timestamping as Int<u32>,
            (SOL_SOCKET, SO_TIMESTAMPING_NEW) => Timestamping as Int<u32>,

            (SOL_NETLINK, NETLINK_CAP_ACK) => NetlinkCapAck as IntBool,
            (SOL_NETLINK, NETLINK_ADD_MEMBERSHIP) => NetlinkAddMembership as Int<u32>,
            (SOL_NETLINK, NETLINK_DROP_MEMBERSHIP) => NetlinkDropMembership as Int<u32>,
            (SOL_NETLINK, NETLINK_EXT_ACK) => NetlinkExtAck as IntBool,
            (SOL_NETLINK, NETLINK_GET_STRICT_CHK) => NetlinkGetStrictChk as IntBool,
            (SOL_NETLINK, NETLINK_PKTINFO) => NetlinkPktinfo as IntBool,

            (PROTO_TCP, TCP_NODELAY) => NoDelay as IntBool,
            (PROTO_TCP, TCP_MAXSEG) => MaxSegment as Int<usize>,
            (PROTO_TCP, TCP_INFO) => TcpInfo,
            (PROTO_TCP, TCP_KEEPIDLE) => KeepIdle as Int<u32>,
            (PROTO_TCP, TCP_KEEPINTVL) => KeepInterval as Int<u32>,
            (PROTO_TCP, TCP_KEEPCNT) => KeepCount as Int<u32>,
            (PROTO_TCP, TCP_FASTOPEN) => TcpFastopen as Int<u32>,

            (PROTO_IP, IP_TTL) => Ttl as Int<u8>,
            (PROTO_IP, IP_RECVERR) => RecvErr as IntBool,
            (PROTO_IP, IP_MTU_DISCOVER) => MtuDiscover as Int<u8>,
            (PROTO_IP, IP_PKTINFO) => IpPktinfo as IntBool,
            (PROTO_IP, IP_RECVTTL) => IpRecvTtl as IntBool,
            (PROTO_IP, IP_RECVTOS) => IpRecvTos as IntBool,
            (PROTO_IP, IP_MULTICAST_TTL) => IpMulticastTtl as Int<u32>,
            (PROTO_IP, IP_RECVFRAGSIZE) => IpRecvFragsize as IntBool,

            (SOL_PACKET, PACKET_STATISTICS) => PacketStatistics as PacketStats,
            (SOL_PACKET, PACKET_ADD_MEMBERSHIP) => PacketAddMembership as PacketMembershipConv,
            (SOL_PACKET, PACKET_DROP_MEMBERSHIP) => PacketDropMembership as PacketMembershipConv,
        }
    }};
    ($dispatch:ident, $in:expr, $($pat:pat => $which:ident $(as $conv:ty)?),* $(,)?) => {
        match $in {
            $(
                $pat => {
                    dispatch!($which $(as $conv)?);
                }
            )*
            (other_level, other_optname) => {
                // Probing unsupported options is normal user-space control
                // flow. Keep it at trace: its frequency is caller-controlled,
                // so even debug would drown useful diagnostics.
                trace!("sockopt rejected: level={other_level}, optname={other_optname}");
                return Err(KError::from(LinuxError::ENOPROTOOPT));
            }
        }
    }
}

/// Encodes Linux's `SO_BINDTODEVICE` getsockopt result.
///
/// An unbound socket returns a zero length without accessing `optval`. A bound
/// socket requires an `IFNAMSIZ`-sized input buffer and returns the actual
/// NUL-terminated interface-name length.
fn encode_bound_device_name(name: Option<&str>, optlen: usize) -> KResult<Vec<u8>> {
    let Some(name) = name else {
        return Ok(Vec::new());
    };
    if optlen < IFNAMSIZ {
        return Err(KError::InvalidInput);
    }

    let name = name.as_bytes();
    let copy_len = name.len().min(IFNAMSIZ - 1);
    let mut buf = vec![0u8; copy_len + 1];
    buf[..copy_len].copy_from_slice(&name[..copy_len]);
    Ok(buf)
}

/// Gets a socket option at a specified protocol level.
///
/// # Errors
///
/// Returns an error if `fd` is invalid or is not a socket, if `level` and
/// `optname` do not name a supported option, if the supplied output length is
/// too small, or if `optval` or `optlen_ptr` cannot be accessed in user memory.
/// Option-specific failures from the underlying socket are also propagated.
pub fn sys_getsockopt(
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserPtr<u8>,
    optlen_ptr: UserPtr<socklen_t>,
) -> KResult<isize> {
    fn put<T>(dst: UserPtr<u8>, len: &mut socklen_t, value: &T) -> KResult<()> {
        if (*len as usize) < size_of::<T>() {
            return Err(KError::InvalidInput);
        }
        *len = size_of::<T>() as socklen_t;
        write_vm_mem(
            dst.cast::<T>().as_ptr().cast_mut(),
            core::slice::from_ref(value),
        )
        .map_err(Into::into)
    }

    /// Writes an `int` option the way Linux `sk_getsockopt` does: it copies
    /// `min(optlen, size_of::<i32>())` bytes and reports back the length it
    /// actually wrote, so a short buffer truncates instead of failing and a
    /// zero length leaves the buffer untouched (and may be a null pointer).
    fn put_truncated(dst: UserPtr<u8>, len: &mut socklen_t, value: i32) -> KResult<()> {
        put_truncated_bytes(dst, len, &value.to_ne_bytes())
    }

    /// The common short-buffer copy used by options whose Linux handler copies
    /// `min(optlen, sizeof(result))` bytes and reports the copied length.
    fn put_truncated_bytes(dst: UserPtr<u8>, len: &mut socklen_t, bytes: &[u8]) -> KResult<()> {
        // `sk_getsockopt` reads the length as a signed `int`; a negative value
        // is rejected before any copy, even when the buffer pointer is valid.
        if (*len as i32) < 0 {
            return Err(KError::InvalidInput);
        }
        let written = (*len as usize).min(bytes.len());
        if written > 0 {
            osvm::write_vm_bytes(dst.as_ptr() as *mut u8, &bytes[..written])?;
        }
        *len = written as socklen_t;
        Ok(())
    }

    /// Copies a Linux `struct ucred` image with the same truncating rule as
    /// `sock_getsockopt` for `SO_PEERCRED`.
    ///
    /// The ABI image is built field by field instead of casting
    /// [`UnixCredentials`], so neither structure padding nor its Rust layout
    /// can leak into userspace.
    fn put_truncated_ucred(
        dst: UserPtr<u8>,
        len: &mut socklen_t,
        value: &UnixCredentials,
    ) -> KResult<()> {
        let mut bytes = [0u8; 3 * size_of::<u32>()];
        for (index, field) in [value.pid, value.uid, value.gid].into_iter().enumerate() {
            let start = index * size_of::<u32>();
            bytes[start..start + size_of::<u32>()].copy_from_slice(&field.to_ne_bytes());
        }
        put_truncated_bytes(dst, len, &bytes)
    }

    // Linux resolves the descriptor first: `__sys_getsockopt` reports `EBADF`
    // or `ENOTSOCK` before the protocol layer runs, and the length is read and
    // rejected inside `sk_getsockopt` only afterwards (net/core/sock.c). The
    // order is observable, so a bad descriptor wins over an unreachable length
    // pointer and over a negative length.
    let file = kprocess::current_files().get_file(fd)?;
    let socket = sock_from_file(&file)?;
    let mut optlen = optlen_ptr.read_vm()?;
    debug!(
        "sys_getsockopt <= fd: {}, level: {}, optname: {}, optlen: {}",
        fd, level, optname, optlen,
    );
    // A `socklen_t` carries the same four bytes as the signed `int` Linux
    // reads, so the sign bit is the test.
    if (optlen as i32) < 0 {
        return Err(KError::InvalidInput);
    }

    if level == PROTO_IP
        && let Some(option) = LegacyGetOption::from_raw(optname)
    {
        option.get(socket.as_ref(), optval, optlen as usize)?;
        optlen_ptr.write_vm(optlen)?;
        return Ok(0);
    }

    macro_rules! dispatch {
        ($which:ident) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::$which(&mut val))?;
            put(optval, &mut optlen, &val)?;
        };
        ($which:ident as $conv:ty) => {
            let mut val = Default::default();
            socket.get_option(GetSocketOption::$which(&mut val))?;
            let sys_val = <$conv>::rust_to_sys(val)?;
            put(optval, &mut optlen, &sys_val)?;
        };
    }
    // `SO_TYPE`, `SO_DOMAIN` and `SO_PROTOCOL` describe the socket itself
    // rather than a stored option: Linux reports `sk_type`, `sk_family` and
    // `sk_protocol` back from `sk_getsockopt` (net/core/sock.c), and runtime
    // supervisors (containerd-shim, ttrpc) use `SO_TYPE` to validate inherited
    // descriptors right after spawn. All three copy with the caller's length,
    // so a short buffer truncates rather than failing.
    let identity = match (level, optname) {
        (SOL_SOCKET, SO_TYPE) => Some(socket.socket_type()),
        (SOL_SOCKET, SO_DOMAIN) => Some(socket.socket_family()),
        (SOL_SOCKET, SO_PROTOCOL) => Some(socket.socket_protocol()),
        _ => None,
    };
    if level == SOL_SOCKET && optname == SO_PEERCRED {
        let mut peer = PeerCredentials::default();
        socket.get_option(GetSocketOption::PeerCredentials(&mut peer))?;
        // `SO_PEERCRED` projects the stored peer PID into the caller's active
        // PID namespace at read time, like Linux `pid_vnr`. A peer that is not
        // visible there reports PID 0 while keeping its real UID and GID.
        let namespace = kprocess::current_user_thread()
            .process()
            .active_pid_namespace();
        let value = peer.resolve_in(&namespace);
        put_truncated_ucred(optval, &mut optlen, &value)?;
    } else if let Some(identity) = identity {
        put_truncated(optval, &mut optlen, identity)?;
    } else if level == SOL_SOCKET && optname == SO_BINDTODEVICE {
        let mut name: Option<alloc::string::String> = None;
        socket.get_option(GetSocketOption::BindToDevice(&mut name))?;
        let buf = encode_bound_device_name(name.as_deref(), optlen as usize)?;
        if !buf.is_empty() {
            osvm::write_vm_bytes(optval.as_ptr() as *mut u8, &buf)?;
        }
        optlen = buf.len() as socklen_t;
    } else if level == SOL_NETLINK && optname == NETLINK_LIST_MEMBERSHIPS {
        optlen = put_list_memberships(&socket, optval, optlen)?;
    } else {
        call_dispatch!(dispatch, (level, optname));
    }

    optlen_ptr.write_vm(optlen)?;
    Ok(0)
}

/// Writes `NETLINK_LIST_MEMBERSHIPS` and returns the new `optlen`.
///
/// `netlink_getsockopt(NETLINK_LIST_MEMBERSHIPS)` copies as much of the socket's
/// group bitmap as fits and reports the bitmap's full byte length, so a caller
/// can size the buffer from a zero-length query. Reporting a zero length for a
/// socket that did join groups would tell it there are no memberships at all.
fn put_list_memberships(
    socket: &Arc<Socket>,
    optval: UserPtr<u8>,
    optlen: socklen_t,
) -> KResult<socklen_t> {
    let mut groups = 0u32;
    socket.get_option(GetSocketOption::NetlinkMemberships(&mut groups))?;
    let bytes = groups.to_ne_bytes();
    // `netlink_getsockopt` copies whole `u32` words only (`if (len - pos <
    // sizeof(u32)) break;`), so a buffer shorter than one word receives nothing
    // while `optlen` still reports the full length for sizing.
    if (optlen as usize) >= size_of::<u32>() {
        osvm::write_vm_bytes(optval.as_ptr() as *mut u8, &bytes)?;
    }
    Ok(bytes.len() as socklen_t)
}

/// Reads an `IP_*` integer option the way `do_ip_setsockopt` does.
///
/// Linux's `IP_*` option handlers accept either a full `int` or a single
/// `unsigned char` (`if (optlen >= sizeof(int)) ... else if (optlen >=
/// sizeof(char))`), because callers commonly pass a boolean as one byte.
fn get_int_or_byte(val: UserConstPtr<u8>, len: socklen_t) -> KResult<i32> {
    if len as usize >= size_of::<i32>() {
        val.cast::<i32>().read_vm().map_err(KError::from)
    } else if len as usize >= size_of::<u8>() {
        let byte = val.read_vm().map_err(KError::from)?;
        Ok(i32::from(byte))
    } else {
        // `do_ip_setsockopt` initialises `val = 0` and only overwrites it for a
        // length of at least one byte, so a zero-length request means "disable"
        // rather than EINVAL.
        Ok(0)
    }
}

/// Applies an `IP_*` option whose argument Linux reads as an `int` *or* a byte.
///
/// `do_ip_setsockopt` handles `IP_PKTINFO`, `IP_RECVTTL`, `IP_RECVTOS`,
/// `IP_RECVFRAGSIZE` and `IP_MULTICAST_TTL` (among others) with `if (optlen >=
/// sizeof(int)) ... else if (optlen >= sizeof(char))`, because callers commonly
/// pass a boolean as one byte. This mirrors that, and reports whether the
/// option was one of them.
fn set_ip_length_tolerant(
    socket: &Arc<Socket>,
    optname: u32,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> KResult<Option<()>> {
    let value = match optname {
        linux_raw_sys::net::IP_PKTINFO
        | linux_raw_sys::net::IP_RECVTTL
        | linux_raw_sys::net::IP_RECVTOS
        | linux_raw_sys::net::IP_RECVFRAGSIZE => get_int_or_byte(optval, optlen)?,
        linux_raw_sys::net::IP_MULTICAST_TTL => get_int_or_byte(optval, optlen)?,
        _ => return Ok(None),
    };
    match optname {
        linux_raw_sys::net::IP_PKTINFO => {
            let enabled = value != 0;
            socket.set_option(SetSocketOption::IpPktinfo(&enabled))?;
        }
        linux_raw_sys::net::IP_RECVTTL => {
            let enabled = value != 0;
            socket.set_option(SetSocketOption::IpRecvTtl(&enabled))?;
        }
        linux_raw_sys::net::IP_RECVTOS => {
            let enabled = value != 0;
            socket.set_option(SetSocketOption::IpRecvTos(&enabled))?;
        }
        linux_raw_sys::net::IP_RECVFRAGSIZE => {
            // `do_ip_setsockopt`: only raw and datagram sockets ever carry
            // fragments, so a stream socket is EINVAL rather than silently
            // tracking a flag it can never report.
            let socket_type = socket.socket_type();
            if socket_type != linux_raw_sys::net::SOCK_RAW as i32
                && socket_type != linux_raw_sys::net::SOCK_DGRAM as i32
            {
                return Err(KError::InvalidInput);
            }
            let enabled = value != 0;
            socket.set_option(SetSocketOption::IpRecvFragsize(&enabled))?;
        }
        linux_raw_sys::net::IP_MULTICAST_TTL => {
            // `do_ip_setsockopt`: a stream socket cannot take it, a zero-length
            // argument is `EINVAL` (this option has an explicit `optlen < 1`
            // check, unlike the boolean ones above), `-1` means "use 1", and
            // anything else outside 0..=255 is `EINVAL`. Treating the value as
            // an unsigned byte would have accepted 256 and rejected -1, which is
            // the common "reset" value.
            if socket.socket_type() == linux_raw_sys::net::SOCK_STREAM as i32 || optlen < 1 {
                return Err(KError::InvalidInput);
            }
            let ttl = if value == -1 { 1 } else { value };
            if !(0..=255).contains(&ttl) {
                return Err(KError::InvalidInput);
            }
            let ttl = ttl as u32;
            socket.set_option(SetSocketOption::IpMulticastTtl(&ttl))?;
        }
        _ => unreachable!(),
    }
    Ok(Some(()))
}

/// Applies `SO_BINDTOIFINDEX` the way `sock_bindtoindex_locked` does.
///
/// The capability check comes first, as in the reference: once a socket is
/// bound to a device, changing or clearing that binding needs `CAP_NET_RAW`.
/// A negative index is `EINVAL` rather than a value to store -- storing it
/// would let an unprivileged caller clear a binding it may not have set and
/// would turn into `ENODEV` when the socket later connects.
fn set_bind_to_ifindex(
    socket: &Arc<Socket>,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> KResult<()> {
    let ifindex: i32 = if optlen as usize != size_of::<i32>() {
        return Err(KError::InvalidInput);
    } else {
        optval.cast::<i32>().read_vm().map_err(KError::from)?
    };
    let mut bound = 0i32;
    socket.get_option(GetSocketOption::BindToIfindex(&mut bound))?;
    if bound != 0 && !kprocess::current_cred().has_cap(kcred::CAP_NET_RAW) {
        return Err(KError::OperationNotPermitted);
    }
    if ifindex < 0 {
        return Err(KError::InvalidInput);
    }
    socket.set_option(SetSocketOption::BindToIfindex(&ifindex))
}

/// Applies one `NETLINK_ADD_MEMBERSHIP` / `NETLINK_DROP_MEMBERSHIP` request.
///
/// `netlink_setsockopt` copies the option value before it switches on the
/// option, so an unreadable buffer is `EFAULT` even for a caller that would then
/// be refused for lack of privilege; the capability check runs after the read.
fn set_group_membership(
    socket: &Arc<Socket>,
    optname: u32,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> KResult<()> {
    // Linux dispatches SOL_NETLINK per family, so a UDP or TCP socket answers
    // `inet_setsockopt`'s default before any value is read: ENOPROTOOPT and no
    // user-memory access. Only a netlink socket reaches the copy below.
    if !socket.is_netlink() {
        return Err(LinuxError::ENOPROTOOPT.into());
    }
    // `netlink_setsockopt` copies `sizeof(int)` bytes when `optlen >=
    // sizeof(int)` and leaves `val` at 0 for a shorter length, so a short
    // request fails the group check with EINVAL rather than the length check.
    let group: u32 = if optlen as usize >= size_of::<u32>() {
        optval.cast::<u32>().read_vm().map_err(KError::from)?
    } else {
        0
    };
    check_group_membership_privilege(socket)?;
    if optname == NETLINK_ADD_MEMBERSHIP {
        socket.set_option(SetSocketOption::NetlinkAddMembership(&group))
    } else {
        socket.set_option(SetSocketOption::NetlinkDropMembership(&group))
    }
}

/// Checks the family and the capability `NETLINK_ADD_MEMBERSHIP` needs.
///
/// Linux's `sock_setsockopt` answers EINVAL for an option a family does not
/// implement, and that precedes any privilege check, so a non-netlink socket is
/// refused as a wrong option rather than as a credentials failure. A netlink
/// socket then goes through `netlink_allowed(sock, NL_CFG_F_NONROOT_RECV)`:
/// the protocol's table decides, or CAP_NET_ADMIN overrides. `NETLINK_USERSOCK`
/// sets only `NL_CFG_F_NONROOT_SEND`, so its groups need the capability, and
/// `ns_capable`'s failure is EPERM.
fn check_group_membership_privilege(socket: &Arc<Socket>) -> KResult<()> {
    if !socket.allows_unprivileged_group_subscribe()
        && !kprocess::current_cred().has_cap(kcred::CAP_NET_ADMIN)
    {
        return Err(KError::OperationNotPermitted);
    }
    Ok(())
}

/// Sets a socket option at a specified protocol level.
///
/// # Errors
///
/// Returns an error if `fd` is invalid or is not a socket, if `level` and
/// `optname` do not name a supported option, if `optlen` is invalid for that
/// option, or if `optval` cannot be read from user memory. Invalid classic-BPF
/// programs and option-specific failures from the underlying socket are also
/// propagated.
pub fn sys_setsockopt(
    fd: i32,
    level: u32,
    optname: u32,
    optval: UserConstPtr<u8>,
    optlen: socklen_t,
) -> KResult<isize> {
    debug!(
        "sys_setsockopt <= fd: {}, level: {}, optname: {}, optlen: {}",
        fd, level, optname, optlen
    );

    fn get<T: UserRead>(val: UserConstPtr<u8>, len: socklen_t) -> KResult<T> {
        if len as usize != size_of::<T>() {
            return Err(KError::InvalidInput);
        }
        val.cast::<T>().read_vm().map_err(KError::from)
    }

    let file = kprocess::current_files().get_file(fd)?;
    let socket = sock_from_file(&file)?;
    // `do_sock_setsockopt` rejects a negative length before any option handler
    // runs, so a `(socklen_t)-1` never reaches an option that treats "large
    // enough" lengths as an `int` read.
    if (optlen as i32) < 0 {
        return Err(KError::InvalidInput);
    }
    if level == PROTO_IP
        && let Some(option) = LegacySetOption::from_raw(optname)
    {
        option.set(socket.as_ref(), optval, optlen as usize)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_ATTACH_FILTER {
        if !supports_kobject_filter_compat(socket.as_ref()) {
            return Err(KError::OperationNotSupported);
        }
        let program = get::<SockFprog>(optval, optlen)?;
        validate_classic_filter(program)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_DETACH_FILTER {
        if !supports_kobject_filter_compat(socket.as_ref()) {
            return Err(KError::OperationNotSupported);
        }
        return Ok(0);
    }
    macro_rules! dispatch {
        ($which:ident) => {
            let val = get(optval, optlen)?;
            socket.set_option(SetSocketOption::$which(&val))?;
        };
        ($which:ident as $conv:ty) => {
            let sys_val = get(optval, optlen)?;
            let mut val = <$conv>::sys_to_rust(sys_val)?;
            socket.set_option(SetSocketOption::$which(&mut val))?;
        };
    }

    if level == SOL_NETLINK && optname == NETLINK_CAP_ACK {
        // Linux `netlink_setsockopt()` treats a short option length as a zero
        // value for NETLINK_CAP_ACK rather than rejecting it: it only reads
        // the integer when `optlen >= sizeof(int)`.
        let enabled = if optlen >= size_of::<i32>() as socklen_t {
            optval.cast::<i32>().read_vm().map_err(KError::from)? != 0
        } else {
            false
        };
        socket.set_option(SetSocketOption::NetlinkCapAck(&enabled))?;
        return Ok(0);
    }
    if level == PROTO_IP && set_ip_length_tolerant(&socket, optname, optval, optlen)?.is_some() {
        return Ok(0);
    }
    if level == SOL_SOCKET
        && (optname == linux_raw_sys::net::SO_SNDBUFFORCE
            || optname == linux_raw_sys::net::SO_RCVBUFFORCE)
    {
        // `sock_setsockopt` gates both force forms on `sockopt_capable(
        // CAP_NET_ADMIN)`: they bypass the `wmem_max`/`rmem_max` clamp, which is
        // exactly the privilege a non-root caller must not have.
        if !kprocess::current_cred().has_cap(kcred::CAP_NET_ADMIN) {
            // `sockopt_capable` returns `capable(cap)`, whose failure is EPERM.
            return Err(KError::OperationNotPermitted);
        }
    }
    if level == SOL_NETLINK
        && (optname == NETLINK_ADD_MEMBERSHIP || optname == NETLINK_DROP_MEMBERSHIP)
    {
        set_group_membership(&socket, optname, optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == linux_raw_sys::net::SO_BINDTOIFINDEX {
        set_bind_to_ifindex(&socket, optval, optlen)?;
        return Ok(0);
    }
    if level == SOL_SOCKET && optname == SO_BINDTODEVICE {
        if optlen == 0 {
            socket.set_option(SetSocketOption::BindToDevice(&None))?;
        } else {
            // Linux `sock_setbindtodevice` copies at most IFNAMSIZ-1 bytes
            // into a zeroed stack buffer; oversized optlen is truncated, not
            // rejected.
            let copy_len = (optlen as usize).min(IFNAMSIZ - 1);
            let buf = osvm::load_vec::<u8>(optval.as_ptr(), copy_len)?;
            let name_str = alloc::string::String::from_utf8(
                buf.iter().copied().take_while(|&b| b != 0).collect(),
            )
            .map_err(|_| KError::InvalidInput)?;
            if name_str.is_empty() {
                socket.set_option(SetSocketOption::BindToDevice(&None))?;
            } else {
                socket.set_option(SetSocketOption::BindToDevice(&Some(name_str)))?;
            }
        }
    } else {
        call_dispatch!(dispatch, (level, optname));
    }

    Ok(0)
}

unittest::path_mod_test!(tests, "../ut-tests/opt.rs");
