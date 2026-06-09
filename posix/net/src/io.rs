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
use kfd::FileLike;
use khal::time::wall_time;
use kio::prelude::*;
use knet::{
    AncillaryData, KernelAncillaryData, RecvFlags, RecvOptions, SendFlags, SendOptions, Socket,
    SocketAddrEx, SocketErrorInfo, SocketOps,
};
use linux_raw_sys::{
    general::timespec,
    net::{
        MSG_CTRUNC, MSG_ERRQUEUE, MSG_PEEK, MSG_TRUNC, SCM_RIGHTS, SOL_SOCKET, cmsghdr, mmsghdr,
        msghdr, sockaddr, socklen_t,
    },
};
use osvm::{VirtPtr, VmBytes, VmBytesMut, write_vm_mem};
use posix_types::{IoVec, IoVectorBuf, TimeValueLike, UserConstPtr, UserPtr};

use crate::{
    addr::SocketAddrExt,
    cmsg::{CMsg, CMsgBuilder, push_ip_recverr_cmsg},
};

// Linux ABI for sendmmsg/recvmmsg limits vlen to UIO_MAXIOV (1024).
const MMSG_MAX_VLEN: u32 = 1024;

fn parse_recvmmsg_timeout(timeout: UserConstPtr<timespec>) -> KResult<Option<Duration>> {
    if timeout.is_null() {
        return Ok(None);
    }
    let tv = timeout.read_vm()?.try_into_time_value()?;
    Ok(Some(Duration::new(tv.as_secs(), tv.subsec_nanos())))
}

fn parse_send_cmsgs(
    resources: &kthread::ProcessResources,
    control_ptr: usize,
    control_len: usize,
) -> KResult<Vec<AncillaryData>> {
    let mut ancillary = Vec::new();
    if control_ptr == 0 || control_len == 0 {
        return Ok(ancillary);
    }

    let mut ptr = control_ptr;
    let ptr_end = ptr.checked_add(control_len).ok_or(KError::InvalidInput)?;

    while let Some(next) = ptr.checked_add(size_of::<cmsghdr>()) {
        if next > ptr_end {
            break;
        }

        let hdr_ptr = UserConstPtr::<cmsghdr>::from(ptr);
        let hdr = hdr_ptr.read_vm()?;
        if hdr.cmsg_len < size_of::<cmsghdr>() || ptr_end - ptr < hdr.cmsg_len {
            return Err(KError::InvalidInput);
        }

        ancillary.push(Box::new(CMsg::parse(resources, hdr_ptr, hdr)?) as AncillaryData);
        ptr += hdr.cmsg_len;
    }

    Ok(ancillary)
}

enum SocketAncillary {
    Rights { fds: Vec<Arc<dyn FileLike>> },
    IpError(SocketErrorInfo),
}

fn into_socket_ancillary(ancillary: AncillaryData) -> Option<SocketAncillary> {
    // `AncillaryData` is the type-erased boundary between `posix-net` and `knet`.
    // Send-side cmsgs carry `posix-net`-owned `CMsg`, while receive-side
    // asynchronous errors are produced by `knet` as `KernelAncillaryData`.
    let type_id = ancillary.as_ref().type_id();
    if type_id == TypeId::of::<CMsg>() {
        let ancillary = ancillary.downcast::<CMsg>().ok()?;
        return Some(match *ancillary {
            CMsg::Rights { fds } => SocketAncillary::Rights { fds },
        });
    }
    if type_id == TypeId::of::<KernelAncillaryData>() {
        let ancillary = ancillary.downcast::<KernelAncillaryData>().ok()?;
        return Some(match *ancillary {
            KernelAncillaryData::IpError(err) => SocketAncillary::IpError(err),
        });
    }

    None
}

fn push_socket_cmsg(
    resources: &kthread::ProcessResources,
    builder: &mut CMsgBuilder<'_>,
    ancillary: SocketAncillary,
) -> KResult<bool> {
    match ancillary {
        SocketAncillary::Rights { fds } => builder.push(SOL_SOCKET, SCM_RIGHTS, |data| {
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
                let fd = resources.add_file_like(f, false)?;
                chunk.copy_from_slice(&fd.to_ne_bytes());
                written += size_of::<i32>();
            }
            Ok(written)
        }),
        SocketAncillary::IpError(err) => push_ip_recverr_cmsg(builder, err),
    }
}

/// Send data on a socket with optional destination address and ancillary data
fn send_impl(
    fd: i32,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    ancillary: Vec<AncillaryData>,
) -> KResult<isize> {
    let addr = if addr.is_null() || addrlen == 0 {
        None
    } else {
        Some(SocketAddrEx::read_from_user(addr, addrlen)?)
    };

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {addr:?}");

    let socket = kthread::current_resources().get_file_like_as::<Socket>(fd)?;
    let sent = socket.send(
        &mut src,
        SendOptions {
            to: addr,
            flags: SendFlags::default(),
            ancillary,
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
    let msg = msg.read_vm()?;
    let resources = kthread::current_resources();
    let ancillary = parse_send_cmsgs(
        resources.as_ref(),
        msg.msg_control as usize,
        msg.msg_controllen,
    )?;
    send_impl(
        fd,
        IoVectorBuf::from_iovecs(IoVec::load_from_user(
            posix_types::UserConstPtr::from(msg.msg_iov as usize),
            msg.msg_iovlen,
        )?)?
        .into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        msg.msg_namelen as socklen_t,
        ancillary,
    )
}

/// Receive data from a socket with optional remote address and ancillary data collection
struct RecvOutput<'a> {
    addr: UserPtr<sockaddr>,
    addrlen: UserPtr<socklen_t>,
    out_flags: Option<&'a mut RecvFlags>,
    cmsg_builder: Option<CMsgBuilder<'a>>,
    msg_flags: Option<&'a mut u32>,
}

impl<'a> RecvOutput<'a> {
    fn new(addr: UserPtr<sockaddr>, addrlen: UserPtr<socklen_t>) -> Self {
        Self {
            addr,
            addrlen,
            out_flags: None,
            cmsg_builder: None,
            msg_flags: None,
        }
    }
}

fn recv_impl(
    fd: i32,
    mut dst: impl Write + IoBufMut,
    flags: u32,
    output: RecvOutput<'_>,
) -> KResult<isize> {
    debug!("sys_recv <= fd: {fd}, flags: {flags}");

    let proc_state = kthread::current_process_state();
    let resources = proc_state.resources.clone();
    let socket = resources.get_file_like_as::<Socket>(fd)?;
    let mut recv_flags = RecvFlags::empty();
    if flags & MSG_PEEK != 0 {
        recv_flags |= RecvFlags::PEEK;
    }
    if flags & MSG_TRUNC != 0 {
        recv_flags |= RecvFlags::TRUNCATE;
    }
    if flags & MSG_ERRQUEUE != 0 {
        recv_flags |= RecvFlags::ERROR_QUEUE;
    }

    let mut ancillary = Vec::new();
    let mut reported_flags = RecvFlags::empty();

    let mut remote_addr =
        (!output.addr.is_null()).then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let recv = socket.recv(
        &mut dst,
        RecvOptions {
            from: remote_addr.as_mut(),
            flags: recv_flags,
            ancillary: Some(&mut ancillary),
            out_flags: Some(&mut reported_flags),
        },
    )?;
    if let Some(out_flags) = output.out_flags {
        *out_flags = reported_flags;
    }

    if let Some(remote_addr) = remote_addr {
        let mut addrlen_value = output.addrlen.read_vm()?;
        remote_addr.write_to_user(output.addr, &mut addrlen_value)?;
        output.addrlen.write_vm(addrlen_value)?;
    }

    let mut cmsg_truncated = false;
    if let Some(mut builder) = output.cmsg_builder {
        for ancillary in ancillary {
            let Some(ancillary) = into_socket_ancillary(ancillary) else {
                warn!("received unexpected ancillary");
                continue;
            };
            let push_result = push_socket_cmsg(resources.as_ref(), &mut builder, ancillary);

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

    if let Some(msg_flags) = output.msg_flags {
        *msg_flags |= recv_truncate_to_msg_flag(reported_flags);
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
        RecvOutput::new(addr, addrlen),
    )
}

/// Receive data with vectored I/O and ancillary data (control messages)
pub fn sys_recvmsg(fd: i32, msg: UserPtr<msghdr>, flags: u32) -> KResult<isize> {
    let mut msg_value = msg.read_vm()?;
    msg_value.msg_flags = 0;
    let result = recv_impl(
        fd,
        IoVectorBuf::from_iovecs(IoVec::load_from_user(
            posix_types::UserConstPtr::from(msg_value.msg_iov as usize),
            msg_value.msg_iovlen,
        )?)?
        .into_io(),
        flags,
        RecvOutput {
            cmsg_builder: (!msg_value.msg_control.is_null()).then(|| {
                CMsgBuilder::new(
                    UserPtr::from(msg_value.msg_control as *mut cmsghdr),
                    &mut msg_value.msg_controllen,
                )
            }),
            msg_flags: Some(&mut msg_value.msg_flags),
            ..RecvOutput::new(
                UserPtr::from(msg_value.msg_name as usize),
                UserPtr::from(&mut msg_value.msg_namelen as *mut _ as *mut socklen_t),
            )
        },
    );
    write_vm_mem(msg.as_ptr().cast_mut(), core::slice::from_ref(&msg_value))?;
    result
}

fn recv_truncate_to_msg_flag(flags: RecvFlags) -> u32 {
    if flags.contains(RecvFlags::TRUNCATE) {
        MSG_TRUNC
    } else {
        0
    }
}

/// Send multiple datagrams in one syscall.
pub fn sys_sendmmsg(fd: i32, msgvec: UserPtr<mmsghdr>, vlen: u32, flags: u32) -> KResult<isize> {
    if vlen == 0 {
        return Ok(0);
    }
    if vlen > MMSG_MAX_VLEN {
        return Err(KError::InvalidInput);
    }

    let mut msgvec_value = msgvec.load_vm_vec(vlen as usize)?;
    let resources = kthread::current_resources();
    let mut sent = 0;
    for msg in msgvec_value.iter_mut() {
        let ancillary = parse_send_cmsgs(
            resources.as_ref(),
            msg.msg_hdr.msg_control as usize,
            msg.msg_hdr.msg_controllen,
        )?;
        match send_impl(
            fd,
            IoVectorBuf::from_iovecs(IoVec::load_from_user(
                posix_types::UserConstPtr::from(msg.msg_hdr.msg_iov as usize),
                msg.msg_hdr.msg_iovlen,
            )?)?
            .into_io(),
            flags,
            UserConstPtr::from(msg.msg_hdr.msg_name as usize),
            msg.msg_hdr.msg_namelen as socklen_t,
            ancillary,
        ) {
            Ok(n) => {
                msg.msg_len = n as u32;
                sent += 1;
            }
            Err(e) => {
                if sent > 0 {
                    write_vm_mem(msgvec.as_ptr().cast_mut(), &msgvec_value)?;
                }
                if sent == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }
    if sent > 0 {
        write_vm_mem(msgvec.as_ptr().cast_mut(), &msgvec_value)?;
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

    let mut msgvec_value = msgvec.load_vm_vec(vlen as usize)?;
    let mut received = 0;
    for msg in msgvec_value.iter_mut() {
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
            IoVectorBuf::from_iovecs(IoVec::load_from_user(
                posix_types::UserConstPtr::from(msg.msg_hdr.msg_iov as usize),
                msg.msg_hdr.msg_iovlen,
            )?)?
            .into_io(),
            flags,
            RecvOutput {
                cmsg_builder: (!msg.msg_hdr.msg_control.is_null()).then(|| {
                    CMsgBuilder::new(
                        UserPtr::from(msg.msg_hdr.msg_control as *mut cmsghdr),
                        &mut msg.msg_hdr.msg_controllen,
                    )
                }),
                msg_flags: Some(&mut msg.msg_hdr.msg_flags),
                ..RecvOutput::new(
                    UserPtr::from(msg.msg_hdr.msg_name as usize),
                    UserPtr::from(&mut msg.msg_hdr.msg_namelen as *mut _ as *mut socklen_t),
                )
            },
        ) {
            Ok(n) => {
                msg.msg_len = n as u32;
                received += 1;
            }
            Err(e) => {
                write_vm_mem(msgvec.as_ptr().cast_mut(), &msgvec_value)?;
                if received == 0 {
                    return Err(e);
                }
                break;
            }
        }
    }

    if received > 0 {
        write_vm_mem(msgvec.as_ptr().cast_mut(), &msgvec_value)?;
    }
    Ok(received)
}
