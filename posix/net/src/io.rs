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

use alloc::{sync::Arc, vec::Vec};
use core::{
    any::TypeId,
    mem::{offset_of, size_of},
    net::Ipv4Addr,
};

use kerrno::{KError, KResult, LinuxError};
use khal::time::monotonic_time;
use kio::prelude::*;
use knet::{
    AncillaryData, KernelAncillaryData, RecvFlags, RecvOptions, SendFlags, SendOptions, Socket,
    SocketAddrEx, SocketErrorInfo, SocketOps, sock_from_file, unix::current_sender_credentials,
};
use ktime_types::TimeSpan;
use kvfs::VfsFile;
use linux_raw_sys::{
    general::timespec,
    net::{
        MSG_CMSG_CLOEXEC, MSG_CTRUNC, MSG_DONTWAIT, MSG_ERRQUEUE, MSG_OOB, MSG_PEEK, MSG_TRUNC,
        SCM_CREDENTIALS, SCM_RIGHTS, SOL_SOCKET, cmsghdr, mmsghdr, msghdr, sockaddr, socklen_t,
    },
};
use osvm::{VirtPtr, VmBytes, VmBytesMut};
use posix_types::{IoVec, IoVectorBuf, TimeSpanLike, UserConstPtr, UserPtr};

use crate::{
    addr::SocketAddrExt,
    cmsg::{
        CMsg, CMsgBuilder, CmsgFamily, OPTMEM_MAX, ScmFdBudget, ScmSenderContext,
        parse_control_msgs, push_ip_recverr_cmsg,
    },
};

// Linux ABI for sendmmsg/recvmmsg limits vlen to UIO_MAXIOV (1024).
const MMSG_MAX_VLEN: u32 = 1024;

fn parse_send_flags(flags: u32) -> SendFlags {
    let mut send_flags = SendFlags::empty();
    if flags & MSG_DONTWAIT != 0 {
        send_flags |= SendFlags::DONT_WAIT;
    }
    if flags & MSG_OOB != 0 {
        send_flags |= SendFlags::OOB;
    }
    send_flags
}

/// Returns a user pointer to the field `offset` bytes into the user value at `base`.
fn field_ptr<S, F>(base: UserPtr<S>, offset: usize) -> UserPtr<F> {
    UserPtr::from(base.as_ptr().addr().wrapping_add(offset))
}

/// Returns a user pointer to entry `index` of the user `mmsghdr` array `msgvec`.
fn mmsg_entry(msgvec: UserPtr<mmsghdr>, index: usize) -> UserPtr<mmsghdr> {
    field_ptr(msgvec, index.wrapping_mul(size_of::<mmsghdr>()))
}

/// Copies the `msghdr` fields that `recvmsg` updates back to the user header.
///
/// Linux writes back only these output fields. Copying the whole header would
/// also copy its implicit padding, which is uninitialized in a kernel value.
fn write_recvmsg_outputs(hdr: UserPtr<msghdr>, msg: &msghdr) -> KResult<()> {
    field_ptr(hdr, offset_of!(msghdr, msg_namelen)).write_vm(msg.msg_namelen)?;
    field_ptr(hdr, offset_of!(msghdr, msg_controllen)).write_vm(msg.msg_controllen)?;
    field_ptr(hdr, offset_of!(msghdr, msg_flags)).write_vm(msg.msg_flags)?;
    Ok(())
}

/// Copies the `recvmsg` output fields and `msg_len` of each received message
/// to the leading entries of the user array `msgvec`.
fn write_recvmmsg_outputs(msgvec: UserPtr<mmsghdr>, msgs: &[mmsghdr]) -> KResult<()> {
    for (index, msg) in msgs.iter().enumerate() {
        let entry = mmsg_entry(msgvec, index);
        write_recvmsg_outputs(field_ptr(entry, offset_of!(mmsghdr, msg_hdr)), &msg.msg_hdr)?;
        field_ptr(entry, offset_of!(mmsghdr, msg_len)).write_vm(msg.msg_len)?;
    }
    Ok(())
}

fn parse_recvmmsg_timeout(timeout: UserConstPtr<timespec>) -> KResult<Option<TimeSpan>> {
    if timeout.is_null() {
        return Ok(None);
    }
    Ok(Some(timeout.read_vm()?.try_into_time_span()?))
}

/// Parsed send-side control data: relayable ancillary records plus an
/// optional validated `SCM_CREDENTIALS` claim.
struct ParsedCmsgs {
    ancillary: Vec<AncillaryData>,
    credentials: Option<knet::SenderCredentials>,
}

/// Maximum control-buffer length accepted on the send path: Linux
/// `sock_kmalloc` also requires the allocation to stay *below*
/// `optmem_max`, so a buffer at the bound itself is rejected.
const CONTROL_BUFFER_LIMIT: usize = OPTMEM_MAX;

/// Parses the send control buffer into relayable ancillary records and an
/// optional validated `SCM_CREDENTIALS` identity.
///
/// # Execution context
///
/// Requires a current user process thread when `family` is Unix or netlink:
/// the `SCM_CREDENTIALS` validator reads the caller's process identity and
/// subjective credentials, and the helper panics otherwise (as with any
/// `kprocess::current_user_thread` caller). Must not run in IRQ context: it
/// copies the whole control buffer from user memory and may allocate, so it
/// may sleep and the caller must not hold a lock the page-fault or allocator
/// path needs. No other caller-held lock is assumed.
fn parse_send_cmsgs(
    resources: &kresources::TaskFiles,
    control_ptr: usize,
    control_len: usize,
    family: CmsgFamily,
) -> KResult<ParsedCmsgs> {
    if control_ptr == 0 || control_len == 0 {
        return Ok(ParsedCmsgs {
            ancillary: Vec::new(),
            credentials: None,
        });
    }
    // Linux rejects a control length above INT_MAX in `____sys_sendmsg` and
    // then caps the snapshot allocation at `optmem_max` in `sock_kmalloc`,
    // both with ENOBUFS.
    if control_len > i32::MAX as usize {
        return Err(KError::from(LinuxError::ENOBUFS));
    }
    if control_len >= CONTROL_BUFFER_LIMIT {
        return Err(KError::from(LinuxError::ENOBUFS));
    }

    // Snapshot the whole control buffer into kernel memory so header
    // validation, body parsing, and fd resolution all see one consistent
    // copy, the way Linux `____sys_sendmsg` copies into `ctl_buf` before
    // `__scm_send` parses it.
    let control = UserConstPtr::<u8>::from(control_ptr).load_vm_vec(control_len)?;
    // `SCM_CREDENTIALS` validation needs the caller's identity for both Unix
    // and netlink; other families ignore both generic SCM message types.
    let sender = matches!(family, CmsgFamily::Unix | CmsgFamily::Netlink).then(|| {
        let thread = kprocess::current_user_thread();
        ScmSenderContext {
            pid: thread.process().visible_pid() as u32,
            cred: kprocess::current_cred(),
            process: thread.process().clone(),
        }
    });

    let mut fd_budget = ScmFdBudget::default();
    let (msgs, credentials) = parse_control_msgs(&control, family, sender.as_ref(), &mut |fds| {
        // Linux caps the descriptors of one `sendmsg` across all of its
        // `SCM_RIGHTS` messages, not only within each.
        fd_budget.charge(fds.len())?;
        let mut files = Vec::with_capacity(fds.len());
        for fd in fds {
            files.push(resources.get_file(fd)?);
        }
        Ok(CMsg::Rights { fds: files })
    })?;
    Ok(ParsedCmsgs {
        ancillary: msgs
            .into_iter()
            .map(|cmsg| Arc::new(cmsg) as AncillaryData)
            .collect(),
        credentials,
    })
}

enum SocketAncillary {
    Rights { fds: Vec<Arc<VfsFile>> },
    IpError(SocketErrorInfo),
    Credentials(knet::SenderCredentials),
}

fn into_socket_ancillary(ancillary: AncillaryData) -> Option<SocketAncillary> {
    // `AncillaryData` is the type-erased boundary between `posix-net` and `knet`.
    // Send-side cmsgs carry `posix-net`-owned `CMsg`, while receive-side
    // asynchronous errors are produced by `knet` as `KernelAncillaryData`. The
    // `Arc` payload is shared; each delivery clones the handle reference so a
    // `MSG_PEEK` duplicates the open file descriptions without reopening them.
    let type_id = (*ancillary).type_id();
    if type_id == TypeId::of::<CMsg>() {
        let ancillary = ancillary.clone().downcast::<CMsg>().ok()?;
        return Some(match ancillary.as_ref() {
            CMsg::Rights { fds } => SocketAncillary::Rights { fds: fds.clone() },
        });
    }
    if type_id == TypeId::of::<KernelAncillaryData>() {
        let ancillary = ancillary.clone().downcast::<KernelAncillaryData>().ok()?;
        return Some(match ancillary.as_ref() {
            KernelAncillaryData::IpError(err) => SocketAncillary::IpError(err.clone()),
            KernelAncillaryData::Credentials(credentials) => {
                SocketAncillary::Credentials(credentials.clone())
            }
        });
    }

    None
}

fn push_socket_cmsg(
    resources: &kprocess::CurrentFiles,
    builder: &mut CMsgBuilder<'_>,
    ancillary: SocketAncillary,
    cloexec: bool,
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
                let fd = resources.add_file(f, cloexec)?;
                chunk.copy_from_slice(&fd.to_ne_bytes());
                written += size_of::<i32>();
            }
            Ok(written)
        }),
        SocketAncillary::IpError(err) => push_ip_recverr_cmsg(builder, err),
        SocketAncillary::Credentials(credentials) => {
            // Linux resolves the sender's `struct pid` in the receiving task's
            // active PID namespace (`pid_vnr`), so a peer in another namespace
            // sees its own view of the sender (0 when there is no projection,
            // which is also the fixed kernel-peer identity).
            let pid = credentials
                .pid
                .as_ref()
                .and_then(|pid| {
                    let observer = kprocess::current_user_thread()
                        .process()
                        .active_pid_namespace();
                    pid.number_in(&observer)
                })
                .unwrap_or(0);
            // SenderCredentials stores initial-namespace ids; render them in
            // the receiving task's user namespace (Linux from_kuid_munged).
            let receiver_ns = kprocess::current_cred().user_ns_arc().clone();
            let uid = receiver_ns
                .map_uid_from_initial(credentials.uid)
                .unwrap_or(65_534);
            let gid = receiver_ns
                .map_gid_from_initial(credentials.gid)
                .unwrap_or(65_534);
            push_credentials_cmsg(builder, &knet::options::UnixCredentials { pid, uid, gid })
        }
    }
}

fn push_credentials_cmsg(
    builder: &mut CMsgBuilder<'_>,
    credentials: &knet::options::UnixCredentials,
) -> KResult<bool> {
    let mut is_complete = false;
    let pushed = builder.push(SOL_SOCKET, SCM_CREDENTIALS, |data| {
        // Linux put_cmsg preserves the header and any credential prefix that
        // fits, even when the buffer ends in the middle of a field.
        let fields = [
            credentials.pid.to_ne_bytes(),
            credentials.uid.to_ne_bytes(),
            credentials.gid.to_ne_bytes(),
        ];
        let bytes = fields.as_flattened();
        let body_len = data.len().min(bytes.len());
        data[..body_len].copy_from_slice(&bytes[..body_len]);
        is_complete = body_len == bytes.len();
        Ok(body_len)
    })?;
    // Report truncation only after the partial message has been written.
    Ok(pushed && is_complete)
}

/// Resolves the descriptor once for a send syscall.
///
/// Linux's `sockfd_lookup_light()` runs before any `msg_name`/iovec copy, so
/// descriptor errors take precedence over malformed message headers.
///
/// # Execution context
///
/// Requires a current user process thread.  `kprocess::current_files()`
/// dereferences `current_user_thread()` and panics for a kernel task or IRQ
/// execution path.  The lookup may block on the process's file-table binding
/// and table locks, so it must run in sleepable process context; callers must
/// not hold a spinlock, an IRQ-disable/preempt-disable guard, or any lock the
/// descriptor-table path needs.  Ordinary send-syscall state may be held.  No
/// user memory is copied here, and the returned file and socket references
/// stay valid for the rest of the syscall.
fn lookup_send_socket(fd: i32) -> KResult<(Arc<VfsFile>, Arc<Socket>)> {
    let file = kprocess::current_files().get_file(fd)?;
    let socket = sock_from_file(&file)?;
    Ok((file, socket))
}

/// Send data on a socket with optional destination address and ancillary data.
fn send_impl(
    fd: i32,
    src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    control_ptr: usize,
    control_len: usize,
) -> KResult<isize> {
    let (file, socket) = lookup_send_socket(fd)?;
    send_impl_resolved(
        fd,
        &file,
        &socket,
        src,
        flags,
        addr,
        addrlen,
        control_ptr,
        control_len,
    )
    .map(|(sent, _total)| sent)
}

/// Sends on an already-resolved socket.
///
/// `sys_sendmsg()` and `sys_sendmmsg()` resolve the descriptor before copying
/// their message headers, matching Linux's `sockfd_lookup_light()` ordering.
#[expect(clippy::too_many_arguments, reason = "mirrors the send syscall inputs")]
fn send_impl_resolved(
    fd: i32,
    file: &VfsFile,
    socket: &Arc<Socket>,
    mut src: impl Read + IoBuf,
    flags: u32,
    addr: UserConstPtr<sockaddr>,
    addrlen: socklen_t,
    control_ptr: usize,
    control_len: usize,
) -> KResult<(isize, usize)> {
    // `sendmmsg()` must stop after a short send, exactly as the Linux loop
    // checks `msg_data_left(&msg_sys)`.  Capture the requested byte count
    // before the protocol consumes any of the source.
    let total = src.remaining();
    // Linux treats a NULL `msg_name` as an absent destination even when
    // `msg_namelen` is nonzero: `__copy_msghdr()` clears both fields together.
    // The same applies to `sendto()`, which shares this path.
    let has_addr = !addr.is_null() && addrlen != 0;

    let resources = kprocess::current_files();
    let family = match socket.as_ref() {
        Socket::Unix(_) => CmsgFamily::Unix,
        Socket::Netlink(_) => CmsgFamily::Netlink,
        _ => CmsgFamily::Other,
    };
    let mut send_flags = parse_send_flags(flags);
    if file.is_nonblocking() {
        send_flags |= SendFlags::DONT_WAIT;
    }
    if matches!(family, CmsgFamily::Netlink) {
        // Linux `netlink_sendmsg` validates these before `scm_send`, unlike
        // Unix which parses control data first.
        if send_flags.contains(SendFlags::OOB) {
            return Err(LinuxError::EOPNOTSUPP.into());
        }
        if src.remaining() == 0 {
            return Err(LinuxError::ENODATA.into());
        }
    }
    // Control data is parsed before the destination for Unix and netlink,
    // matching their `scm_send` ordering. `SCM_RIGHTS`/`SCM_CREDENTIALS`
    // follow the socket family's rules.
    let ParsedCmsgs {
        ancillary,
        credentials,
    } = parse_send_cmsgs(&resources, control_ptr, control_len, family)?;
    let addr = if has_addr {
        Some(SocketAddrEx::read_from_user(addr, addrlen)?)
    } else {
        None
    };

    debug!("sys_send <= fd: {fd}, flags: {flags}, addr: {addr:?}");

    // Linux netlink pre-fills `scm.creds` with the current process TGID and
    // real UID/GID before parsing cmsgs; Unix attaches a record only when the
    // receiver enabled `SO_PASSCRED`. An explicit valid `SCM_CREDENTIALS`
    // replaces that default in both cases.
    let mut sender_credentials = match (family, socket.as_ref()) {
        (CmsgFamily::Netlink, _) => current_sender_credentials(),
        (CmsgFamily::Unix, Socket::Unix(unix)) if unix.should_attach_sender_credentials() => {
            current_sender_credentials()
        }
        _ => None,
    };
    if credentials.is_some() {
        sender_credentials = credentials;
    }
    let options = SendOptions {
        to: addr,
        flags: send_flags,
        ancillary,
        sender_credentials,
        // A destinationless netlink command addressed to the kernel requires
        // the capability in both the opener and the current sender.  The
        // generic VFS socket path passes the same fact through its file.
        opener_cred: Some(file.cred().as_ref().clone()),
    };
    let sent = match socket.as_ref() {
        Socket::Netlink(_) => {
            let sender = kprocess::current_cred();
            socket.send_with_cred(&mut src, options, &sender)?
        }
        _ => socket.send(&mut src, options)?,
    };

    Ok((sent as isize, total))
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
    send_impl(fd, VmBytes::new(buf, len), flags, addr, addrlen, 0, 0)
}

/// Clamps a `sendmsg`/`sendmmsg` destination length the way `__copy_msghdr()`
/// does before `move_addr_to_kernel()` sees it.
///
/// `sendto()` passes its raw `addrlen` to `move_addr_to_kernel()`, which
/// rejects anything larger than `sockaddr_storage`; `sendmsg()` instead
/// clamps `msg_namelen` first, so only that entry point accepts an oversized
/// destination length.  Negative lengths are rejected here: converting them to
/// `socklen_t` would instead turn them into huge positive lengths and let a
/// protocol-specific precheck report a different errno before the address
/// layer sees them.
///
/// The clamp is deliberately generic: family parsers still apply their own
/// address bounds. In particular, `UnixAddr::read_from_user()` rejects lengths
/// at or below `offsetof(struct sockaddr_un, sun_path)` and lengths above
/// `sizeof(struct sockaddr_un)`, matching `unix_validate_addr()`. AF_UNIX
/// lengths of 111 through 128 therefore fail with `EINVAL` after the clamp
/// rather than being accepted as destinations.
fn clamp_sendmsg_name_len(len_bytes: i32) -> KResult<socklen_t> {
    if len_bytes < 0 {
        return Err(LinuxError::EINVAL.into());
    }
    Ok(len_bytes.min(crate::addr::MAX_SOCKADDR_LEN as i32) as socklen_t)
}

/// Send data with vectored I/O and ancillary data (control messages)
pub fn sys_sendmsg(fd: i32, msg: UserConstPtr<msghdr>, flags: u32) -> KResult<isize> {
    // Linux `__sys_sendmsg()` resolves the descriptor before
    // `sendmsg_copy_msghdr()`, so EBADF/ENOTSOCK precedes EINVAL from a
    // negative destination length or a fault from the header/iovec copy.
    let (file, socket) = lookup_send_socket(fd)?;
    let msg = msg.read_vm()?;
    // Linux `__copy_msghdr()` validates the destination length before it
    // imports the iovec, and a NULL name clears the length without inspecting
    // it.  Preserve both rules so a negative length is EINVAL here, while a
    // NULL name with any length remains an addressless send.
    let name_len = if msg.msg_name.is_null() {
        0
    } else {
        clamp_sendmsg_name_len(msg.msg_namelen)?
    };
    send_impl_resolved(
        fd,
        &file,
        &socket,
        IoVectorBuf::from_iovecs(IoVec::load_from_user(
            posix_types::UserConstPtr::from(msg.msg_iov as usize),
            msg.msg_iovlen,
        )?)?
        .into_io(),
        flags,
        UserConstPtr::from(msg.msg_name as usize),
        name_len,
        msg.msg_control as usize,
        msg.msg_controllen,
    )
    .map(|(sent, _total)| sent)
}

/// Receive data from a socket with optional remote address and ancillary data collection
struct RecvOutput<'a> {
    addr: UserPtr<sockaddr>,
    addrlen: RecvAddrLen<'a>,
    out_flags: Option<&'a mut RecvFlags>,
    cmsg_builder: Option<CMsgBuilder<'a>>,
    msg_flags: Option<&'a mut u32>,
}

enum RecvAddrLen<'a> {
    User(UserPtr<socklen_t>),
    Value(&'a mut socklen_t),
}

impl RecvAddrLen<'_> {
    fn read(&self) -> KResult<socklen_t> {
        match self {
            RecvAddrLen::User(ptr) => ptr.read_vm().map_err(Into::into),
            RecvAddrLen::Value(value) => Ok(**value),
        }
    }

    fn write(&mut self, value: socklen_t) -> KResult<()> {
        match self {
            RecvAddrLen::User(ptr) => ptr.write_vm(value).map_err(Into::into),
            RecvAddrLen::Value(dst) => {
                **dst = value;
                Ok(())
            }
        }
    }
}

impl<'a> RecvOutput<'a> {
    fn new(addr: UserPtr<sockaddr>, addrlen: UserPtr<socklen_t>) -> Self {
        Self {
            addr,
            addrlen: RecvAddrLen::User(addrlen),
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
    mut output: RecvOutput<'_>,
) -> KResult<isize> {
    debug!("sys_recv <= fd: {fd}, flags: {flags}");

    let resources = kprocess::current_files();
    let file = resources.get_file(fd)?;
    let socket = sock_from_file(&file)?;
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
    if file.is_nonblocking() || flags & MSG_DONTWAIT != 0 {
        recv_flags |= RecvFlags::DONT_WAIT;
    }
    if flags & MSG_OOB != 0 {
        recv_flags |= RecvFlags::OOB;
    }

    let mut ancillary = Vec::new();
    let mut reported_flags = RecvFlags::empty();
    let mut ancillary_discarded = false;

    // A Unix socket without a control buffer must let the transport decide the
    // fate of the arrived control data: on a stream the read reaches a pending
    // control record, so the record must be dropped instead of being kept for a
    // later `recvmsg` (which would inherit stale ancillary data), and on a
    // datagram the message's descriptors are dropped along with it. Both report
    // the loss through `ancillary_discarded` for `MSG_CTRUNC`. Other socket
    // types keep populating the vector because a caller can still recover flags
    // or diagnostic data from it.
    let ancillary_sink =
        if output.cmsg_builder.is_none() && matches!(socket.as_ref(), Socket::Unix(_)) {
            None
        } else {
            Some(&mut ancillary)
        };
    let mut remote_addr =
        (!output.addr.is_null()).then(|| SocketAddrEx::Ip((Ipv4Addr::UNSPECIFIED, 0).into()));
    let recv = socket.recv(
        &mut dst,
        RecvOptions {
            from: remote_addr.as_mut(),
            flags: recv_flags,
            ancillary: ancillary_sink,
            out_flags: Some(&mut reported_flags),
            ancillary_discarded: Some(&mut ancillary_discarded),
        },
    )?;
    if let Some(out_flags) = output.out_flags {
        *out_flags = reported_flags;
    }

    if let Some(remote_addr) = remote_addr {
        let mut addrlen_value = output.addrlen.read()?;
        if matches!(
            remote_addr,
            SocketAddrEx::Unix(knet::unix::UnixAddr::Unbound)
        ) {
            // Unlike getsockname, recvfrom reports no name for an unbound sender.
            addrlen_value = 0;
        } else {
            remote_addr.write_to_user(output.addr, &mut addrlen_value)?;
        }
        output.addrlen.write(addrlen_value)?;
    }

    let mut cmsg_truncated = output.cmsg_builder.is_none() && !ancillary.is_empty();
    if let Some(mut builder) = output.cmsg_builder {
        for ancillary in ancillary {
            let Some(ancillary) = into_socket_ancillary(ancillary) else {
                warn!("received unexpected ancillary");
                continue;
            };
            let push_result = push_socket_cmsg(
                &resources,
                &mut builder,
                ancillary,
                flags & MSG_CMSG_CLOEXEC != 0,
            );

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
        *msg_flags |= flags & (MSG_CMSG_CLOEXEC | MSG_ERRQUEUE);
        if cmsg_truncated || ancillary_discarded {
            // `scm_detach_fds` reports MSG_CTRUNC whenever fd-carrying control
            // data could not be emitted, including the empty-buffer case:
            // `scm_max_fds` yields zero and the arrived fds are dropped.
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
    if msg_value.msg_control.is_null() {
        msg_value.msg_controllen = 0;
    }
    let mut msg_namelen = msg_value.msg_namelen as socklen_t;
    let result = recv_impl(
        fd,
        IoVectorBuf::from_iovecs(IoVec::load_from_user(
            posix_types::UserConstPtr::from(msg_value.msg_iov as usize),
            msg_value.msg_iovlen,
        )?)?
        .into_io(),
        flags,
        RecvOutput {
            addr: UserPtr::from(msg_value.msg_name as usize),
            addrlen: RecvAddrLen::Value(&mut msg_namelen),
            cmsg_builder: (!msg_value.msg_control.is_null()).then(|| {
                CMsgBuilder::new(
                    UserPtr::from(msg_value.msg_control as *mut cmsghdr),
                    &mut msg_value.msg_controllen,
                )
            }),
            msg_flags: Some(&mut msg_value.msg_flags),
            out_flags: None,
        },
    );
    msg_value.msg_namelen = msg_namelen as _;
    write_recvmsg_outputs(msg, &msg_value)?;
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
    // Linux clamps `vlen` before the one descriptor lookup, and still performs
    // that lookup when `vlen == 0`; `sendmmsg(-1, ..., 0)` is EBADF, not zero.
    let (file, socket) = lookup_send_socket(fd)?;
    if vlen == 0 {
        return Ok(0);
    }
    let vlen = vlen.min(MMSG_MAX_VLEN);

    let mut sent = 0;
    for index in 0..vlen as usize {
        let entry = mmsg_entry(msgvec, index);
        // Linux reads and sends one `mmsghdr` at a time.  A later header or
        // iovec fault ends the call while preserving previously sent
        // datagrams and their `msg_len` outputs.
        let result = (|| -> KResult<(isize, usize)> {
            let header: msghdr = field_ptr(entry, offset_of!(mmsghdr, msg_hdr)).read_vm()?;
            let name_len = if header.msg_name.is_null() {
                0
            } else {
                clamp_sendmsg_name_len(header.msg_namelen)?
            };
            send_impl_resolved(
                fd,
                &file,
                &socket,
                IoVectorBuf::from_iovecs(IoVec::load_from_user(
                    posix_types::UserConstPtr::from(header.msg_iov as usize),
                    header.msg_iovlen,
                )?)?
                .into_io(),
                flags,
                UserConstPtr::from(header.msg_name as usize),
                name_len,
                header.msg_control as usize,
                header.msg_controllen,
            )
        })();
        match result {
            Ok((n, total)) => {
                // Linux increments the returned datagram count only after
                // storing `msg_len`; a fault while writing the count discards
                // this message from the prefix just like a send failure.
                if let Err(error) =
                    field_ptr(entry, offset_of!(mmsghdr, msg_len)).write_vm(n as u32)
                {
                    if sent == 0 {
                        return Err(error.into());
                    }
                    break;
                }
                sent += 1;
                // A protocol may accept fewer bytes than the iovecs offered.
                // Linux treats that as a completed datagram and stops the
                // `sendmmsg` loop rather than sending the remaining entries.
                if (n as usize) < total {
                    break;
                }
            }
            Err(error) => {
                if sent == 0 {
                    return Err(error);
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
    let deadline = timeout.and_then(|span| monotonic_time().checked_add(span));

    let mut msgvec_value = msgvec.load_vm_vec(vlen as usize)?;
    let mut received = 0;
    for msg in msgvec_value.iter_mut() {
        if let Some(deadline) = deadline
            && monotonic_time() >= deadline
        {
            if received == 0 {
                return Err(KError::WouldBlock);
            }
            break;
        }
        msg.msg_hdr.msg_flags = 0;
        if msg.msg_hdr.msg_control.is_null() {
            msg.msg_hdr.msg_controllen = 0;
        }
        let mut msg_namelen = msg.msg_hdr.msg_namelen as socklen_t;
        let result = recv_impl(
            fd,
            IoVectorBuf::from_iovecs(IoVec::load_from_user(
                posix_types::UserConstPtr::from(msg.msg_hdr.msg_iov as usize),
                msg.msg_hdr.msg_iovlen,
            )?)?
            .into_io(),
            flags,
            RecvOutput {
                addr: UserPtr::from(msg.msg_hdr.msg_name as usize),
                addrlen: RecvAddrLen::Value(&mut msg_namelen),
                cmsg_builder: (!msg.msg_hdr.msg_control.is_null()).then(|| {
                    CMsgBuilder::new(
                        UserPtr::from(msg.msg_hdr.msg_control as *mut cmsghdr),
                        &mut msg.msg_hdr.msg_controllen,
                    )
                }),
                msg_flags: Some(&mut msg.msg_hdr.msg_flags),
                out_flags: None,
            },
        );
        msg.msg_hdr.msg_namelen = msg_namelen as _;
        match result {
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

    write_recvmmsg_outputs(msgvec, &msgvec_value[..received as usize])?;
    Ok(received)
}
