// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket option syscalls.
//!
//! This module implements socket option manipulation including:
//! - Get socket options (getsockopt, etc.)
//! - Set socket options (setsockopt, etc.)
//! - Socket-level, IP-level, TCP-level, and other protocol options

use kerrno::{KError, KResult, LinuxError};
use knet::{
    options::{Configurable, GetSocketOption, SetSocketOption},
    sock_from_file,
};
use linux_raw_sys::net::socklen_t;
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr, UserRead};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;

const PACKET_ADD_MEMBERSHIP: u32 = 1;
const PACKET_DROP_MEMBERSHIP: u32 = 2;
const PACKET_STATISTICS: u32 = 6;

mod conv {
    use kerrno::{KError, KResult};
    use knet::options::{PacketMembership, PacketStatistics, UnixCredentials};
    use linux_raw_sys::{general::timeval, net::ucred};
    use posix_types::{TimeSpanLike, UserRead};

    pub struct Int<T>(T);

    impl<T: TryFrom<i32> + TryInto<i32>> Int<T> {
        pub fn sys_to_rust(val: i32) -> KResult<T> {
            T::try_from(val).map_err(|_| KError::InvalidInput)
        }

        pub fn rust_to_sys(val: T) -> KResult<i32> {
            val.try_into().map_err(|_| KError::InvalidInput)
        }
    }

    pub struct IntBool;

    impl IntBool {
        pub fn sys_to_rust(val: i32) -> KResult<bool> {
            Ok(val != 0)
        }

        pub fn rust_to_sys(val: bool) -> KResult<i32> {
            Ok(val as _)
        }
    }

    pub struct Duration;

    impl Duration {
        pub fn sys_to_rust(val: timeval) -> KResult<ktime_types::TimeSpan> {
            val.try_into_time_span()
        }

        pub fn rust_to_sys(val: ktime_types::TimeSpan) -> KResult<timeval> {
            Ok(timeval::from_time_span(val))
        }
    }

    pub struct Ucred;

    impl Ucred {
        pub fn sys_to_rust(val: ucred) -> KResult<UnixCredentials> {
            Ok(UnixCredentials {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }

        pub fn rust_to_sys(val: UnixCredentials) -> KResult<ucred> {
            Ok(ucred {
                pid: val.pid,
                uid: val.uid,
                gid: val.gid,
            })
        }
    }

    #[allow(non_camel_case_types)]
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct tpacket_stats {
        pub tp_packets: u32,
        pub tp_drops: u32,
    }

    // SAFETY: `tpacket_stats` is a POD getsockopt result structure.
    unsafe impl UserRead for tpacket_stats {}

    pub struct PacketStats;

    impl PacketStats {
        pub fn sys_to_rust(val: tpacket_stats) -> KResult<PacketStatistics> {
            Ok(PacketStatistics {
                packets: val.tp_packets,
                drops: val.tp_drops,
            })
        }

        pub fn rust_to_sys(val: PacketStatistics) -> KResult<tpacket_stats> {
            Ok(tpacket_stats {
                tp_packets: val.packets,
                tp_drops: val.drops,
            })
        }
    }

    #[allow(non_camel_case_types)]
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct packet_mreq {
        pub mr_ifindex: i32,
        pub mr_type: u16,
        pub mr_alen: u16,
        pub mr_address: [u8; 8],
    }

    // SAFETY: `packet_mreq` is a POD setsockopt/getsockopt carrier.
    unsafe impl UserRead for packet_mreq {}

    pub struct PacketMembershipConv;

    impl PacketMembershipConv {
        pub fn sys_to_rust(val: packet_mreq) -> KResult<PacketMembership> {
            Ok(PacketMembership {
                ifindex: val.mr_ifindex,
                membership_type: val.mr_type,
                addr_len: val.mr_alen,
                addr: val.mr_address,
            })
        }

        pub fn rust_to_sys(val: PacketMembership) -> KResult<packet_mreq> {
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
            (SOL_SOCKET, SO_SNDBUF) => SendBuffer as Int<usize>,
            (SOL_SOCKET, SO_RCVBUF) => ReceiveBuffer as Int<usize>,
            (SOL_SOCKET, SO_KEEPALIVE) => KeepAlive as IntBool,
            (SOL_SOCKET, SO_RCVTIMEO) => ReceiveTimeout as Duration,
            (SOL_SOCKET, SO_SNDTIMEO) => SendTimeout as Duration,
            (SOL_SOCKET, SO_PASSCRED) => PassCredentials as IntBool,
            (SOL_SOCKET, SO_PEERCRED) => PeerCredentials as Ucred,

            (PROTO_TCP, TCP_NODELAY) => NoDelay as IntBool,
            (PROTO_TCP, TCP_MAXSEG) => MaxSegment as Int<usize>,
            (PROTO_TCP, TCP_INFO) => TcpInfo,

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

/// Get socket options at a specified protocol level
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

    let file = kprocess::current_resources().get_file(fd)?;
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
    call_dispatch!(dispatch, (level, optname));

    optlen_ptr.write_vm(optlen)?;
    Ok(0)
}

/// Set socket options at a specified protocol level
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

    let file = kprocess::current_resources().get_file(fd)?;
    let socket = sock_from_file(&file)?;
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

    call_dispatch!(dispatch, (level, optname));

    Ok(0)
}
