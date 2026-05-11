// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket option syscalls.
//!
//! This module implements socket option manipulation including:
//! - Get socket options (getsockopt, etc.)
//! - Set socket options (setsockopt, etc.)
//! - Socket-level, IP-level, TCP-level, and other protocol options

use core::mem::MaybeUninit;

use kerrno::{KError, KResult, LinuxError};
use knet::options::{Configurable, GetSocketOption, SetSocketOption};
use kservices::file::Socket;
use linux_raw_sys::net::socklen_t;
use osvm::{VirtPtr, read_vm_mem, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr};

const PROTO_TCP: u32 = linux_raw_sys::net::IPPROTO_TCP as u32;

const PROTO_IP: u32 = linux_raw_sys::net::IPPROTO_IP as u32;

mod conv {
    use kerrno::{KError, KResult};
    use knet::options::UnixCredentials;
    use linux_raw_sys::{general::timeval, net::ucred};
    use posix_types::TimeValueLike;

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
        pub fn sys_to_rust(val: timeval) -> KResult<core::time::Duration> {
            val.try_into_time_value()
        }

        pub fn rust_to_sys(val: core::time::Duration) -> KResult<timeval> {
            Ok(timeval::from_time_value(val))
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

    let socket = kthread::current_resources().get_file_like_as::<Socket>(fd)?;
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

    fn get<T: Copy>(val: UserConstPtr<u8>, len: socklen_t) -> KResult<T> {
        if len as usize != size_of::<T>() {
            return Err(KError::InvalidInput);
        }
        let mut value = MaybeUninit::<T>::uninit();
        read_vm_mem(val.cast::<T>().as_ptr(), core::slice::from_mut(&mut value))
            .map_err(KError::from)?;
        Ok(unsafe { value.assume_init() })
    }

    let socket = kthread::current_resources().get_file_like_as::<Socket>(fd)?;
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
