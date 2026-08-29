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
use linux_raw_sys::net::{SO_BINDTODEVICE, SOL_SOCKET, socklen_t};
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr, UserRead};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;

const PACKET_ADD_MEMBERSHIP: u32 = 1;
const PACKET_DROP_MEMBERSHIP: u32 = 2;
const PACKET_STATISTICS: u32 = 6;

const IFNAMSIZ: usize = 16;

mod conv {
    use kerrno::{KError, KResult};
    use knet::options::{PacketMembership, PacketStatistics, UnixCredentials};
    use ktime_types::{NANOS_PER_SEC, TimeSpan};
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

    pub struct SocketTimeout;

    impl SocketTimeout {
        pub fn sys_to_rust(val: timeval) -> KResult<TimeSpan> {
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

        pub fn rust_to_sys(val: TimeSpan) -> KResult<timeval> {
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

#[cfg(unittest)]
mod tests {
    use kerrno::KError;
    use ktime_types::{NANOS_PER_SEC, TimeSpan};
    use linux_raw_sys::general::timeval;
    use unittest::{assert_eq, def_test};

    use super::{IFNAMSIZ, conv::SocketTimeout, encode_bound_device_name};

    const NANOS_PER_TICK: u64 = NANOS_PER_SEC / kbuild_config::TICKS_PER_SECOND as u64;

    #[def_test]
    fn unbound_device_get_accepts_zero_length() {
        let encoded = encode_bound_device_name(None, 0).unwrap();

        assert_eq!(encoded.len(), 0);
        assert_eq!(encode_bound_device_name(None, IFNAMSIZ).unwrap().len(), 0);
    }

    #[def_test]
    fn bound_device_get_requires_ifnamsiz_and_returns_actual_length() {
        assert_eq!(
            encode_bound_device_name(Some("eth0"), IFNAMSIZ - 1).unwrap_err(),
            KError::InvalidInput
        );

        let encoded = encode_bound_device_name(Some("eth0"), IFNAMSIZ).unwrap();
        assert_eq!(encoded.as_slice(), b"eth0\0");
    }

    #[def_test]
    fn zero_socket_timeout_remains_infinite() {
        let timeout = SocketTimeout::sys_to_rust(timeval {
            tv_sec: 0,
            tv_usec: 0,
        })
        .unwrap();

        assert_eq!(timeout, TimeSpan::ZERO);
    }

    /// Regression test for <https://gitee.com/openkylin/x-kernel/issues/IK97G7>.
    #[def_test]
    fn one_microsecond_socket_timeout_rounds_up_to_one_tick() {
        let timeout = SocketTimeout::sys_to_rust(timeval {
            tv_sec: 0,
            tv_usec: 1,
        })
        .unwrap();

        assert_eq!(timeout, TimeSpan::from_nanos(NANOS_PER_TICK));
    }

    #[def_test]
    fn exact_tick_socket_timeout_is_unchanged() {
        let timeout = SocketTimeout::sys_to_rust(timeval {
            tv_sec: 0,
            tv_usec: (NANOS_PER_TICK / 1_000) as _,
        })
        .unwrap();

        assert_eq!(timeout, TimeSpan::from_nanos(NANOS_PER_TICK));
    }
}
