// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network I/O syscalls.
//!
//! This module implements network I/O operations including:
//! - Send and receive (send, recv, sendto, recvfrom, etc.)
//! - Vectored I/O (sendmsg, recvmsg, etc.)
//! - Out-of-band data handling
//! - Ancillary data (control messages)

use alloc::{boxed::Box, vec::Vec};
use core::{net::Ipv4Addr, time::Duration};

use kerrno::{KError, KResult};
use kio::prelude::*;
use knet::{
    CMsgData, RecvFlags, RecvOptions, SendFlags, SendOptions, SocketAddrEx, SocketOps,
    options::{Configurable, GetSocketOption, SetSocketOption},
};
use kservices::mm::{UserConstPtr, UserPtr, VmBytes, VmBytesMut};
use linux_raw_sys::{
    general::timespec,
    net::{
        MSG_PEEK, MSG_TRUNC, SCM_RIGHTS, SOL_SOCKET, cmsghdr, mmsghdr, msghdr, sockaddr, socklen_t,
    },
};

// Linux ABI for sendmmsg/recvmmsg limits vlen to UIO_MAXIOV (1024).
const MMSG_MAX_VLEN: u32 = 1024;

fn parse_recvmmsg_timeout(timeout: UserConstPtr<timespec>) -> KResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let ts = timeout.get_as_ref()?;
    let tv = (*ts).try_into_time_value()?;
    Ok(Some(Duration::new(tv.as_secs(), tv.subsec_nanos())))
}

fn parse_send_cmsgs(control_ptr: usize, control_len: usize) -> KResult<Vec<CMsgData>> {
    let mut cmsg = Vec::new();
    if control_ptr == 0 || control_len == 0 {
        return Ok(cmsg);
    }

    let mut ptr = control_ptr;
    let ptr_end = ptr.checked_add(control_len).ok_or(KError::InvalidInput)?;

    while let Some(next) = ptr.checked_add(size_of::<cmsghdr>()) {
        if next > ptr_end {
            break;
        }

        let hdr = UserConstPtr::<cmsghdr>::from(ptr).get_as_ref()?;
        if hdr.cmsg_len < size_of::<cmsghdr>() || ptr_end - ptr < hdr.cmsg_len {
            return Err(KError::InvalidInput);
        }

        cmsg.push(Box::new(CMsg::parse(hdr)?) as CMsgData);
        ptr += hdr.cmsg_len;
    }

    Ok(cmsg)
}
use posix_types::TimeValueLike;

use crate::{
    file::{FileLike, Socket, add_file_like},
    io::{IoVec, IoVectorBuf},
    net::{CMsg, CMsgBuilder},
    socket::SocketAddrExt,
};

/// Send data on a socket with optional destination address and ancillary data
fn send_impl(
    fd: i32,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    cmsg: Vec<CMsgData>,
) -> KResult<isize> {
    let addr = if addr.is_null() || addrlen == 0 {
        None
    } else {
        Some(SocketAddrEx::read_from_user(addr, addrlen)?)
    };

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {addr:?}");

    let socket = Socket::from_fd(fd)?;
    let sent = socket.send(
        &mut src,
        SendOptions {
            to: addr,
            flags: SendFlags::default(),
            cmsg,
        },
    )?;

    Ok(sent as isize)
}

/// Send data to a specific address on a socket
pub fn sys_sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
) -> KResult<isize> {
    send_impl(fd, VmBytes::new(buf, len), flags, addr, addrlen, Vec::new())
}

/// Send data with vectored I/O and ancillary data (control messages)
pub fn sys_sendmsg(fd: i32, msg: UserConstPtr<msghdr>, flags: u32) -> KResult<isize> {
    let msg = msg.get_as_ref()?;
    let cmsg = parse_send_cmsgs(msg.msg_control as usize, msg.msg_controllen)?;
    send_impl(
        fd,
        IoVectorBuf::new(msg.msg_iov as *const IoVec, msg.msg_iovlen)?.into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        cmsg,
    )
}

/// Receive data from a socket with optional remote address and ancillary data collection
fn recv_impl(
    fd: i32,
    mut dst: impl Write + IoBufMut,
    flags: u32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
    cmsg_builder: Option<CMsgBuilder>,
) -> KResult<isize> {
    debug!("sys_recv <= fd: {fd}, flags: {flags}");

    let socket = Socket::from_fd(fd)?;
    let mut recv_flags = RecvFlags::empty();
    if flags & MSG_PEEK != 0 {
        recv_flags |= RecvFlags::PEEK;
    }
    if flags & MSG_TRUNC != 0 {
        recv_flags |= RecvFlags::TRUNCATE;
    }

    let mut cmsg = Vec::new();

    let mut remote_addr =
        (!addr.is_null()).then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let recv = socket.recv(
        &mut dst,
        RecvOptions {
            from: remote_addr.as_mut(),
            flags: recv_flags,
            cmsg: Some(&mut cmsg),
        },
    )?;

    if let Some(remote_addr) = remote_addr {
        remote_addr.write_to_user(addr, addrlen.get_as_mut()?)?;
    }

    if let Some(mut builder) = cmsg_builder {
        for cmsg in cmsg {
            let Ok(cmsg) = cmsg.downcast::<CMsg>() else {
                warn!("received unexpected cmsg");
                continue;
            };

            let pushed = match *cmsg {
                CMsg::Rights { fds } => builder.push(SOL_SOCKET, SCM_RIGHTS, |data| {
                    let mut written = 0;
                    for (f, chunk) in fds.into_iter().zip(data.chunks_exact_mut(size_of::<i32>())) {
                        let fd = add_file_like(f, false)?;
                        chunk.copy_from_slice(&fd.to_ne_bytes());
                        written += size_of::<i32>();
                    }
                    Ok(written)
                })?,
            };
            if !pushed {
                break;
            }
        }
    }

    debug!("sys_recv => fd: {fd}, recv: {recv}");
    Ok(recv as isize)
}

/// Receive data from a socket with the sender's address
pub fn sys_recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: u32,
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
) -> KResult<isize> {
    recv_impl(fd, VmBytesMut::new(buf, len), flags, addr, addrlen, None)
}

/// Receive data with vectored I/O and ancillary data (control messages)
pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> KResult<isize> {
    let msg = msg.get_as_mut()?;
    recv_impl(
        fd,
        IoVectorBuf::new(msg.msg_iov as *mut IoVec, msg.msg_iovlen)?.into_io(),
        flags,
        UserPtr::from(msg.msg_name as usize),
        UserPtr::from(&mut msg.msg_namelen as *mut _ as *mut socklen_t),
        (!msg.msg_control.is_null()).then(|| {
            CMsgBuilder::new(
                UserPtr::from(msg.msg_control as *mut cmsghdr),
                &mut msg.msg_controllen,
            )
        }),
    )
}

/// Send multiple datagrams in one syscall.
pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> KResult<isize> {
    if vlen == 0 || vlen > MMSG_MAX_VLEN {
        return Err(KError::InvalidInput);
    }

    let msgvec = msgvec.get_as_mut_slice(vlen as usize)?;
    let mut sent = 0;
    for msg in msgvec.iter_mut() {
        let cmsg = parse_send_cmsgs(msg.msg_hdr.msg_control as usize, msg.msg_hdr.msg_controllen)?;
        match send_impl(
            fd,
            IoVectorBuf::new(msg.msg_hdr.msg_iov as *const IoVec, msg.msg_hdr.msg_iovlen)?
                .into_io(),
            flags,
            UserConstPtr::from(msg.msg_hdr.msg_name as usize),
            msg.msg_hdr.msg_namelen as socklen_t,
            cmsg,
        ) {
            Ok(n) => {
                msg.msg_len = n as u32;
                sent += 1;
            }
            Err(e) => {
                if sent == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }
    Ok(sent)
}

/// Receive multiple datagrams in one syscall.
pub fn sys_recvmmsg(
    fd: i32,
    msgvec: UserPtr<mmsghdr>,
    vlen: u32,
    flags: u32,
    timeout: UserConstPtr<timespec>,
) -> KResult<isize> {
    if vlen == 0 || vlen > MMSG_MAX_VLEN {
        return Err(KError::InvalidInput);
    }

    let timeout = parse_recvmmsg_timeout(timeout)?;
    let socket = Socket::from_fd(fd)?;
    let mut old_timeout = Duration::from_nanos(0);
    socket.get_option(GetSocketOption::ReceiveTimeout(&mut old_timeout))?;

    if let Some(timeout) = timeout {
        socket.set_option(SetSocketOption::ReceiveTimeout(&timeout))?;
    }

    let msgvec = msgvec.get_as_mut_slice(vlen as usize)?;
    let mut received = 0;
    let result = (|| {
        for msg in msgvec.iter_mut() {
            match recv_impl(
                fd,
                IoVectorBuf::new(msg.msg_hdr.msg_iov as *mut IoVec, msg.msg_hdr.msg_iovlen)?
                    .into_io(),
                flags,
                UserPtr::from(msg.msg_hdr.msg_name as usize),
                UserPtr::from(&mut msg.msg_hdr.msg_namelen as *mut _ as *mut socklen_t),
                (!msg.msg_hdr.msg_control.is_null()).then(|| {
                    CMsgBuilder::new(
                        UserPtr::from(msg.msg_hdr.msg_control as *mut cmsghdr),
                        &mut msg.msg_hdr.msg_controllen,
                    )
                }),
            ) {
                Ok(n) => {
                    msg.msg_len = n as u32;
                    received += 1;
                }
                Err(e) => {
                    if received == 0 {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        Ok(received)
    })();

    if timeout.is_some() {
        socket.set_option(SetSocketOption::ReceiveTimeout(&old_timeout))?;
    }

    result
}
