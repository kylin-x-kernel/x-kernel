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

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult, LinuxError};
use knet::{
    options::{Configurable, GetSocketOption, SetSocketOption},
    sock_from_file,
};
use linux_raw_sys::net::{
    SO_ATTACH_FILTER, SO_BINDTODEVICE, SO_DETACH_FILTER, SOL_SOCKET, socklen_t,
};
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr, UserRead};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;

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
    use knet::options::{PacketMembership, PacketStatistics, UnixCredentials};
    use ktime_types::{NANOS_PER_SEC, TimeSpan};
    use linux_raw_sys::{general::timeval, net::ucred};
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

    pub(super) struct Ucred;

    impl Ucred {
        pub(super) fn sys_to_rust(val: ucred) -> KResult<UnixCredentials> {
            Ok(UnixCredentials {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }

        pub(super) fn rust_to_sys(val: UnixCredentials) -> KResult<ucred> {
            Ok(ucred {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
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
            (SOL_SOCKET, SO_KEEPALIVE) => KeepAlive as IntBool,
            (SOL_SOCKET, SO_RCVTIMEO) => ReceiveTimeout as SocketTimeout,
            (SOL_SOCKET, SO_SNDTIMEO) => SendTimeout as SocketTimeout,
            (SOL_SOCKET, SO_PASSCRED) => PassCredentials as IntBool,
            (SOL_SOCKET, SO_PEERCRED) => PeerCredentials as Ucred,

            (PROTO_TCP, TCP_NODELAY) => NoDelay as IntBool,
            (PROTO_TCP, TCP_MAXSEG) => MaxSegment as Int<usize>,
            (PROTO_TCP, TCP_INFO) => TcpInfo,
            (PROTO_TCP, TCP_KEEPIDLE) => KeepIdle as Int<u32>,

            (PROTO_IP, IP_TTL) => Ttl as Int<u8>,
            (PROTO_IP, IP_RECVERR) => RecvErr as IntBool,
            (PROTO_IP, IP_MTU_DISCOVER) => MtuDiscover as Int<u8>,

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
            _ => return Err(KError::from(LinuxError::ENOPROTOOPT)),
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
    let mut optlen = optlen_ptr.read_vm()?;
    debug!(
        "sys_getsockopt <= fd: {}, level: {}, optname: {}, optlen: {}",
        fd, level, optname, optlen,
    );

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

    let file = kprocess::current_files().get_file(fd)?;
    let socket = sock_from_file(&file)?;

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
    if level == SOL_SOCKET && optname == SO_BINDTODEVICE {
        let mut name: Option<alloc::string::String> = None;
        socket.get_option(GetSocketOption::BindToDevice(&mut name))?;
        let buf = encode_bound_device_name(name.as_deref(), optlen as usize)?;
        if !buf.is_empty() {
            osvm::write_vm_bytes(optval.as_ptr() as *mut u8, &buf)?;
        }
        optlen = buf.len() as socklen_t;
    } else {
        call_dispatch!(dispatch, (level, optname));
    }

    optlen_ptr.write_vm(optlen)?;
    Ok(0)
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
