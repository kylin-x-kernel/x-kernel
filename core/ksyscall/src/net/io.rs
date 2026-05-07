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

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{any::TypeId, net::Ipv4Addr, time::Duration};

use kerrno::{KError, KResult, LinuxError};
use khal::time::wall_time;
use kio::prelude::*;
use knet::{
    CMsgData, KernelCmsg, RecvFlags, RecvOptions, SendFlags, SendOptions, SocketAddrEx, SocketOps,
    UdpRecvError,
};
use kservices::mm::{UserConstPtr, UserPtr, VmBytes, VmBytesMut};
use linux_raw_sys::{
    general::timespec,
    net::{
        MSG_CTRUNC, MSG_ERRQUEUE, MSG_PEEK, MSG_TRUNC, SCM_RIGHTS, SOL_SOCKET, cmsghdr, mmsghdr,
        msghdr, sockaddr, socklen_t,
    },
};
use posix_types::TimeValueLike;

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

use crate::{
    file::{FileLike, Socket, add_file_like},
    io::{IoVec, IoVectorBuf},
    net::{CMsg, CMsgBuilder},
    socket::SocketAddrExt,
};

enum SocketCmsg {
    Rights { fds: Vec<Arc<dyn FileLike>> },
    IpRecvError(UdpRecvError),
}

fn into_socket_cmsg(cmsg: CMsgData) -> Option<SocketCmsg> {
    // `CMsgData` is the type-erased boundary between `ksyscall` and `knet`.
    // Send-side cmsgs carry `ksyscall`-owned `CMsg`, while receive-side
    // asynchronous errors are produced by `knet` as `KernelCmsg`.
    let type_id = cmsg.as_ref().type_id();
    if type_id == TypeId::of::<CMsg>() {
        let cmsg = cmsg.downcast::<CMsg>().ok()?;
        return Some(match *cmsg {
            CMsg::Rights { fds } => SocketCmsg::Rights { fds },
        });
    }
    if type_id == TypeId::of::<KernelCmsg>() {
        let cmsg = cmsg.downcast::<KernelCmsg>().ok()?;
        return Some(match *cmsg {
            KernelCmsg::IpRecvError(err) => SocketCmsg::IpRecvError(err),
        });
    }

    None
}

fn push_socket_cmsg(builder: &mut CMsgBuilder<'_>, cmsg: SocketCmsg) -> KResult<bool> {
    match cmsg {
        SocketCmsg::Rights { fds } => builder.push(SOL_SOCKET, SCM_RIGHTS, |data| {
            let body_len = fds
                .len()
                .checked_mul(size_of::<i32>())
                .ok_or(KError::from(LinuxError::ENOBUFS))?;
            if data.len() < body_len {
                return Err(KError::from(LinuxError::ENOBUFS));
            }

            let mut written = 0;
            for (f, chunk) in fds
                .into_iter()
                .zip(data[..body_len].chunks_exact_mut(size_of::<i32>()))
            {
                let fd = add_file_like(f, false)?;
                chunk.copy_from_slice(&fd.to_ne_bytes());
                written += size_of::<i32>();
            }
            Ok(written)
        }),
        SocketCmsg::IpRecvError(err) => crate::net::push_ip_recverr_cmsg(builder, err),
    }
}

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
    msg_flags: Option<&mut u32>,
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
    if flags & MSG_ERRQUEUE != 0 {
        recv_flags |= RecvFlags::ERRQUEUE;
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

    let mut cmsg_truncated = false;
    if let Some(mut builder) = cmsg_builder {
        for cmsg in cmsg {
            let Some(cmsg) = into_socket_cmsg(cmsg) else {
                warn!("received unexpected cmsg");
                continue;
            };
            let push_result = push_socket_cmsg(&mut builder, cmsg);

            match push_result {
                Ok(true) => {}
                Ok(false) => {
                    cmsg_truncated = true;
                    break;
                }
                Err(e) if e == KError::from(LinuxError::ENOBUFS) => {
                    cmsg_truncated = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    if let Some(msg_flags) = msg_flags {
        if flags & MSG_ERRQUEUE != 0 {
            *msg_flags |= MSG_ERRQUEUE;
        }
        if cmsg_truncated {
            *msg_flags |= MSG_CTRUNC;
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
    recv_impl(
        fd,
        VmBytesMut::new(buf, len),
        flags,
        addr,
        addrlen,
        None,
        None,
    )
}

/// Receive data with vectored I/O and ancillary data (control messages)
pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> KResult<isize> {
    let msg = msg.get_as_mut()?;
    msg.msg_flags = 0;
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
        Some(&mut msg.msg_flags),
    )
}

/// Send multiple datagrams in one syscall.
pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> KResult<isize> {
    if vlen == 0 {
        return Ok(0);
    }
    if vlen > MMSG_MAX_VLEN {
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
    if vlen == 0 {
        return Ok(0);
    }
    if vlen > MMSG_MAX_VLEN {
        return Err(KError::InvalidInput);
    }

    let timeout = parse_recvmmsg_timeout(timeout)?;
    // TODO: deadline is only checked between recv_impl calls. If a single
    // recv_impl blocks waiting for data (socket has nothing to read), the
    // deadline cannot interrupt it. Needs a non-blocking recv path or
    // SO_RCVTIMEO support at the socket layer to fix.
    let deadline = timeout.map(|t| wall_time() + t);

    let msgvec = msgvec.get_as_mut_slice(vlen as usize)?;
    let mut received = 0;
    for msg in msgvec.iter_mut() {
        if let Some(deadline) = deadline
            && wall_time() >= deadline
        {
            if received == 0 {
                return Err(KError::WouldBlock);
            }
            break;
        }
        msg.msg_hdr.msg_flags = 0;
        match recv_impl(
            fd,
            IoVectorBuf::new(msg.msg_hdr.msg_iov as *mut IoVec, msg.msg_hdr.msg_iovlen)?.into_io(),
            flags,
            UserPtr::from(msg.msg_hdr.msg_name as usize),
            UserPtr::from(&mut msg.msg_hdr.msg_namelen as *mut _ as *mut socklen_t),
            (!msg.msg_hdr.msg_control.is_null()).then(|| {
                CMsgBuilder::new(
                    UserPtr::from(msg.msg_hdr.msg_control as *mut cmsghdr),
                    &mut msg.msg_hdr.msg_controllen,
                )
            }),
            Some(&mut msg.msg_hdr.msg_flags),
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
}
