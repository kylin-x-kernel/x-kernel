// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Control message handling for socket operations.
//!
//! This module provides parsing and handling of control messages (ancillary data)
//! in socket I/O operations, including file descriptor passing and other protocol-specific data.

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    ffi::{c_int, c_long},
    mem::size_of,
    net::SocketAddr,
    ptr,
};

use bytemuck::{NoUninit, bytes_of};
use kerrno::{KError, KResult, LinuxError};
use knet::{SocketErrorInfo, SocketErrorOrigin};
use kvfs::VfsFile;
use linux_raw_sys::net::{
    AF_INET, AF_UNSPEC, IP_RECVERR, IPPROTO_IP, SCM_CREDENTIALS, SCM_RIGHTS, SOL_IP, SOL_IPV6,
    SOL_SOCKET, cmsghdr, in_addr, sockaddr_in, ucred,
};
use osvm::VirtPtr;
use posix_types::UserPtr;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockExtendedErr {
    ee_errno: u32,
    ee_origin: u8,
    ee_type: u8,
    ee_code: u8,
    ee_pad: u8,
    ee_info: u32,
    ee_data: u32,
}

const _: [u8; 16] = [0; size_of::<SockExtendedErr>()];

// SAFETY: `SockExtendedErr` is a `repr(C)` Linux ABI structure made only of
// integer fields. Its field order is `u32, u8, u8, u8, u8, u32, u32`, which
// leaves no padding bytes; the size assertion above keeps this ABI expectation
// checked at compile time.
unsafe impl NoUninit for SockExtendedErr {}

const SO_EE_ORIGIN_LOCAL: u8 = 1;
const SO_EE_ORIGIN_ICMP: u8 = 2;
const SO_EE_ORIGIN_ICMP6: u8 = 3;
const SO_EE_ORIGIN_TXSTATUS: u8 = 4;

fn sock_extended_err_origin(origin: SocketErrorOrigin) -> u8 {
    match origin {
        SocketErrorOrigin::Local => SO_EE_ORIGIN_LOCAL,
        SocketErrorOrigin::Icmp => SO_EE_ORIGIN_ICMP,
        SocketErrorOrigin::Icmp6 => SO_EE_ORIGIN_ICMP6,
        SocketErrorOrigin::TxStatus => SO_EE_ORIGIN_TXSTATUS,
    }
}

fn write_sock_extended_err(data: &mut [u8], err: &SockExtendedErr) -> KResult<()> {
    if data.len() < size_of::<SockExtendedErr>() {
        return Err(KError::from(LinuxError::ENOBUFS));
    }
    data[..size_of::<SockExtendedErr>()].copy_from_slice(bytes_of(err));
    Ok(())
}

fn write_sockaddr_in(data: &mut [u8], addr: Option<SocketAddr>) -> KResult<()> {
    if data.len() < size_of::<sockaddr_in>() {
        return Err(KError::from(LinuxError::ENOBUFS));
    }
    let sa = match addr {
        Some(SocketAddr::V4(addr)) => sockaddr_in {
            sin_family: AF_INET as _,
            sin_port: addr.port().to_be(),
            sin_addr: in_addr {
                s_addr: u32::from_be_bytes(addr.ip().octets()).to_be(),
            },
            __pad: [0; 8],
        },
        _ => sockaddr_in {
            sin_family: AF_UNSPEC as _,
            sin_port: 0,
            sin_addr: in_addr { s_addr: 0 },
            __pad: [0; 8],
        },
    };

    // SAFETY: `sockaddr_in` is fully initialized above and `data` has been
    // checked to fit it at runtime. The ancillary payload is a byte slice, so its
    // address is not guaranteed to satisfy `sockaddr_in` alignment.
    unsafe {
        ptr::write_unaligned(data.as_mut_ptr().cast::<sockaddr_in>(), sa);
    }
    Ok(())
}

fn write_ip_recverr(
    data: &mut [u8],
    header: &SockExtendedErr,
    offender: Option<SocketAddr>,
) -> KResult<usize> {
    debug_assert_eq!(size_of::<SockExtendedErr>(), 16);
    debug_assert_eq!(size_of::<sockaddr_in>(), 16);

    let body_len = size_of::<SockExtendedErr>() + size_of::<sockaddr_in>();
    if data.len() < body_len {
        return Err(KError::from(LinuxError::ENOBUFS));
    }

    let (err_buf, addr_buf) = data[..body_len].split_at_mut(size_of::<SockExtendedErr>());
    write_sock_extended_err(err_buf, header)?;
    write_sockaddr_in(addr_buf, offender)?;
    Ok(body_len)
}

pub(crate) fn push_ip_recverr_cmsg(
    builder: &mut CMsgBuilder<'_>,
    err: SocketErrorInfo,
) -> KResult<bool> {
    builder.push(IPPROTO_IP as u32, IP_RECVERR, |data| {
        let header = SockExtendedErr {
            ee_errno: err.errno.into_raw() as u32,
            ee_origin: sock_extended_err_origin(err.origin),
            ee_type: err.error_type,
            ee_code: err.error_code,
            ee_pad: 0,
            ee_info: err.info,
            ee_data: err.data,
        };

        write_ip_recverr(data, &header, err.offender)
    })
}

/// Maximum number of file descriptors one `sendmsg` may pass, matching Linux
/// `SCM_MAX_FD`. A single `SCM_RIGHTS` message and the total across the
/// message's control messages are both capped.
const SCM_MAX_FD: usize = 253;

/// Descriptor budget for one `sendmsg`, mirroring Linux's `struct scm_fp_list`:
/// the total `SCM_RIGHTS` descriptors across the message's control messages may
/// not exceed [`SCM_MAX_FD`].
#[derive(Default)]
pub(crate) struct ScmFdBudget {
    charged: usize,
}

impl ScmFdBudget {
    /// Charges one control message's descriptor count, rejecting a total above
    /// the `SCM_MAX_FD` cap.
    pub(crate) fn charge(&mut self, count: usize) -> KResult<()> {
        let charged = self.charged + count;
        if charged > SCM_MAX_FD {
            return Err(KError::InvalidInput);
        }
        self.charged = charged;
        Ok(())
    }
}

/// Control message types for socket operations (ancillary data)
pub(crate) enum CMsg {
    /// SCM_RIGHTS: file descriptor passing between processes
    Rights { fds: Vec<Arc<VfsFile>> },
}

/// Upper bound for one control buffer: Linux `sysctl_optmem_max`'s default of
/// 128 KiB (`net/core/net_namespace.c`). `sock_kmalloc`, which backs the
/// control snapshot in `____sys_sendmsg`, requires the allocation to stay
/// below this bound and returns `ENOBUFS` otherwise, so the same bound gates
/// the snapshot here.
pub(crate) const OPTMEM_MAX: usize = 128 * 1024;

/// Alignment granularity of `cmsghdr` on the Linux ABI. Linux defines
/// `CMSG_ALIGN` on `sizeof(long)` (`include/linux/socket.h`), so every header
/// after the first starts at a `cmsg_len` rounded up to this unit.
const CMSG_ALIGN_UNIT: usize = size_of::<c_long>();

/// Rounds `len` up to the `cmsghdr` alignment granularity (Linux
/// `CMSG_ALIGN`), rejecting overflow.
pub(crate) fn cmsg_align(len: usize) -> KResult<usize> {
    len.checked_next_multiple_of(CMSG_ALIGN_UNIT)
        .ok_or(KError::InvalidInput)
}

/// The identity a sender may claim in an `SCM_CREDENTIALS` control message.
///
/// Linux `scm_check_creds` compares the claim against the caller's subjective
/// credentials and TGID, so the parser carries both instead of reading them
/// itself.
pub(crate) struct ScmSenderContext {
    /// The sender's TGID as its PID namespace reports it (Linux
    /// `task_tgid_vnr`); `getpid(2)` in the same namespace returns this value.
    pub(crate) pid: u32,
    /// The sender's subjective credentials.
    pub(crate) cred: Arc<kcred::Cred>,
    /// The sender process identity. A validated claim is this process's own
    /// credentials, so delivery resolves the pid in the receiver's PID
    /// namespace at receive time (Linux `pid_vnr`) instead of forwarding the
    /// sender-namespace number.
    pub(crate) process: Arc<kprocess::Process>,
}

/// Reads a `cmsghdr` from the head of a kernel-side control buffer.
///
/// # SAFETY
///
/// The caller must guarantee `bytes` holds at least `size_of::<cmsghdr>()`
/// bytes. `read_unaligned` tolerates any buffer alignment, so the only safety
/// requirement is the byte count.
fn read_cmsghdr(bytes: &[u8]) -> cmsghdr {
    debug_assert!(bytes.len() >= size_of::<cmsghdr>());
    // SAFETY: `bytes` is a live kernel slice with at least one full header of
    // initialized bytes, and `read_unaligned` imposes no alignment constraint.
    unsafe { ptr::read_unaligned(bytes.as_ptr().cast::<cmsghdr>()) }
}

/// Parses one control buffer snapshot in a single pass, mirroring Linux
/// `__scm_send` over the kernel-side copy that `____sys_sendmsg` takes:
///
/// - headers advance by `cmsg_len` rounded up to [`CMSG_ALIGN_UNIT`]
///   (`__cmsg_nxthdr`), and iteration stops when the next full header does not
///   fit instead of failing;
/// - a header shorter than `cmsghdr`, or whose `cmsg_len` overruns the buffer,
///   is `EINVAL`;
/// - non-`SOL_SOCKET` levels are skipped;
/// - unknown `SOL_SOCKET` types are `EINVAL`.
///
/// Each `SCM_RIGHTS` body is handed to `resolve_rights` while the walk is on
/// that message, so a bad descriptor reports `EBADF` before a malformed later
/// message can turn the failure into `EINVAL`, matching Linux's in-loop
/// `scm_fp_copy`. Validated `SCM_CREDENTIALS` claims are not records to relay:
/// Linux consumes them into `scm->creds`, so the last claim wins and is
/// Socket family policy for `SOL_SOCKET` control messages.
///
/// Linux runs the generic `__scm_send` walk for Unix and netlink sockets only;
/// IP and other families use `sock_cmsg_send`, which has different handling.
/// Keeping the family explicit lets the parser reject `SCM_RIGHTS` on netlink
/// while still accepting `SCM_CREDENTIALS` there.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum CmsgFamily {
    Unix,
    Netlink,
    Other,
}

impl CmsgFamily {
    fn accepts_scm_credentials(self) -> bool {
        matches!(self, Self::Unix | Self::Netlink)
    }
}

/// Parses and validates the control buffer of one send syscall.
///
/// This mirrors Linux's two control-message paths:
///
/// - Unix and netlink sockets use the generic `__scm_send` walk. Both accept
///   `SCM_CREDENTIALS` (netlink pre-fills the current task's credentials before
///   parsing, so an explicit claim replaces them); `SCM_RIGHTS` is accepted only
///   on Unix and is `EINVAL` on netlink. Unknown `SOL_SOCKET` types are
///   `EINVAL`, and non-`SOL_SOCKET` levels are ignored.
/// - Other families use `sock_cmsg_send` semantics here only to the extent the
///   transports support them: `SCM_RIGHTS` and `SCM_CREDENTIALS` are ignored
///   and IPv4/IPv6 packet-control data is rejected because it is not applied.
///
/// `sender` is the identity captured at the syscall boundary. It is `Some` for
/// Unix and netlink send paths that run on a user thread; a missing identity
/// makes an explicit `SCM_CREDENTIALS` claim invalid rather than silently
/// dropping it.
pub(crate) fn parse_control_msgs(
    control: &[u8],
    family: CmsgFamily,
    sender: Option<&ScmSenderContext>,
    resolve_rights: &mut dyn FnMut(Vec<i32>) -> KResult<CMsg>,
) -> KResult<(Vec<CMsg>, Option<knet::SenderCredentials>)> {
    let mut msgs = Vec::new();
    let mut credentials = None;
    let mut offset = 0usize;
    while let Some(remaining) = control.len().checked_sub(offset) {
        // A truncated final header ends the walk, the way `__cmsg_nxthdr`
        // returns NULL when the next header does not fit.
        if remaining < size_of::<cmsghdr>() {
            break;
        }
        let hdr = read_cmsghdr(&control[offset..]);
        let body_end = offset
            .checked_add(hdr.cmsg_len)
            .ok_or(KError::InvalidInput)?;
        if hdr.cmsg_len < size_of::<cmsghdr>() || body_end > control.len() {
            return Err(KError::InvalidInput);
        }

        if hdr.cmsg_level == SOL_SOCKET as c_int {
            let body = &control[offset + size_of::<cmsghdr>()..body_end];
            match hdr.cmsg_type as u32 {
                SCM_RIGHTS if family == CmsgFamily::Unix => {
                    msgs.push(resolve_rights(parse_rights_fds(body)?)?);
                }
                // Linux `__scm_send` rejects file-descriptor passing on every
                // non-Unix family while it walks the control buffer.
                SCM_RIGHTS if family == CmsgFamily::Netlink => {
                    return Err(KError::InvalidInput);
                }
                SCM_CREDENTIALS if family.accepts_scm_credentials() => {
                    let sender = sender.ok_or(KError::InvalidInput)?;
                    credentials = Some(check_creds(body, sender)?);
                }
                // Other families do not run the generic SCM walk. Preserve the
                // existing behavior of ignoring these two types there.
                SCM_RIGHTS | SCM_CREDENTIALS => {}
                _ => return Err(KError::InvalidInput),
            }
        } else if family == CmsgFamily::Other
            && (hdr.cmsg_level == SOL_IP as c_int || hdr.cmsg_level == SOL_IPV6 as c_int)
        {
            // Linux applies IP-level control data (`ip_cmsg_send` /
            // `ip6_cmsg_send`). Without that support here, failing is honest.
            return Err(KError::InvalidInput);
        }

        let space = cmsg_align(hdr.cmsg_len)?;
        offset = offset.checked_add(space).ok_or(KError::InvalidInput)?;
    }
    Ok((msgs, credentials))
}

/// Validates and converts one `SCM_RIGHTS` body (Linux `scm_fp_copy`'s length
/// arithmetic, plus a whole-descriptor check).
fn parse_rights_fds(body: &[u8]) -> KResult<Vec<i32>> {
    let count = CMsg::rights_fd_count(body.len())?;
    let mut fds = Vec::with_capacity(count);
    for fd in body[..count * size_of::<i32>()].chunks_exact(size_of::<i32>()) {
        #[expect(
            clippy::unwrap_used,
            reason = "CG-021-no-panic-on-reachable-failure: chunks_exact(size_of::<i32>()) yields \
                      exactly the four bytes required by i32::from_ne_bytes"
        )]
        fds.push(i32::from_ne_bytes(fd.try_into().unwrap()));
    }
    Ok(fds)
}

/// Validates an `SCM_CREDENTIALS` claim (Linux `scm_check_creds`).
///
/// The claimed pid must be the sender's own TGID unless the caller is
/// privileged in the PID namespace, and each claimed id must be one the caller
/// could adopt with `setuid`/`setgid` unless it holds `CAP_SETUID`/
/// `CAP_SETGID`.
fn check_creds(body: &[u8], sender: &ScmSenderContext) -> KResult<knet::SenderCredentials> {
    if body.len() != size_of::<ucred>() {
        return Err(KError::InvalidInput);
    }
    // SAFETY: `body` has exactly `size_of::<ucred>()` initialized bytes and
    // `read_unaligned` imposes no alignment constraint.
    let claim = unsafe { ptr::read_unaligned(body.as_ptr().cast::<ucred>()) };
    // Linux `make_kuid`/`make_kgid` map `(uid_t)-1` to an invalid id even in
    // the initial user namespace and `scm_check_creds` rejects it with EINVAL
    // before any authorization check.
    if claim.uid == u32::MAX || claim.gid == u32::MAX {
        return Err(KError::InvalidInput);
    }
    let cred = &sender.cred;
    // Linux `make_kuid`/`make_kgid` translate the claimed local ids to global
    // credentials; an id unmapped in the sender's namespace is EINVAL.
    let uid = cred
        .user_ns()
        .map_uid_to_initial(claim.uid)
        .ok_or(KError::from(LinuxError::EINVAL))?;
    let gid = cred
        .user_ns()
        .map_gid_to_initial(claim.gid)
        .ok_or(KError::from(LinuxError::EINVAL))?;
    let pid_ns = sender.process.active_pid_namespace();
    let pid_ok = claim.pid == sender.pid || cred.has_cap_in(pid_ns.owner(), kcred::CAP_SYS_ADMIN);
    let uid_ok = [cred.ruid(), cred.euid(), cred.suid()].contains(&uid)
        || cred.has_cap_in(cred.user_ns(), kcred::CAP_SETUID);
    let gid_ok = [cred.rgid(), cred.egid(), cred.sgid()].contains(&gid)
        || cred.has_cap_in(cred.user_ns(), kcred::CAP_SETGID);
    if !(pid_ok && uid_ok && gid_ok) {
        return Err(KError::from(LinuxError::EPERM));
    }
    // Linux links the claim to a `struct pid`: the sender's own when the
    // numbers match, otherwise `find_get_pid` in the caller's active PID
    // namespace (reachable only with CAP_SYS_ADMIN, as checked above), and a
    // missing process is ESRCH. Delivery later resolves that identity in the
    // receiver's namespace.
    let identity = if claim.pid == sender.pid {
        sender.process.pid_identity()
    } else {
        let observer = sender.process.active_pid_namespace();
        kprocess::Process::pid_identity_in_namespace(claim.pid, &observer)
            .ok_or(KError::NoSuchProcess)?
    };
    Ok(knet::SenderCredentials {
        pid: Some(identity),
        uid,
        gid,
    })
}

impl CMsg {
    /// Returns the descriptor count of one `SCM_RIGHTS` control body, rejecting
    /// a body that is not a whole number of `i32`s or that exceeds the
    /// per-message `SCM_MAX_FD` cap.
    fn rights_fd_count(body_len: usize) -> KResult<usize> {
        if !body_len.is_multiple_of(size_of::<i32>()) {
            return Err(KError::InvalidInput);
        }
        let count = body_len / size_of::<i32>();
        if count > SCM_MAX_FD {
            return Err(KError::InvalidInput);
        }
        Ok(count)
    }
}

/// Builder for constructing control message buffers for socket I/O
pub(crate) struct CMsgBuilder<'a> {
    hdr: UserPtr<cmsghdr>,
    len: &'a mut usize,
    capacity: usize,
}
impl<'a> CMsgBuilder<'a> {
    /// Create a new control message builder with a given buffer and capacity
    pub(crate) fn new(msg: UserPtr<cmsghdr>, len: &'a mut usize) -> Self {
        let capacity = *len;
        *len = 0;
        Self {
            hdr: msg,
            len,
            capacity,
        }
    }

    /// Add a control message with the specified level and type to the buffer
    pub(crate) fn push(
        &mut self,
        level: u32,
        ty: u32,
        body: impl FnOnce(&mut [u8]) -> KResult<usize>,
    ) -> KResult<bool> {
        let Some(remaining) = self.capacity.checked_sub(*self.len) else {
            return Ok(false);
        };
        let Some(body_capacity) = remaining.checked_sub(size_of::<cmsghdr>()) else {
            return Ok(false);
        };

        let mut data = vec![0u8; body_capacity];
        let body_len = body(&mut data)?;

        let cmsg_len = size_of::<cmsghdr>() + body_len;
        UserPtr::<u8>::from(self.hdr.as_ptr() as usize + size_of::<cmsghdr>())
            .write_vm_slice(&data[..body_len])?;

        let hdr = cmsghdr {
            cmsg_len,
            cmsg_level: level as _,
            cmsg_type: ty as _,
        };
        self.hdr.write_vm(hdr)?;

        // cmsg_len excludes trailing padding, but the next header must start
        // at a CMSG_ALIGN boundary. A final message may fit without all of
        // its trailing padding.
        let space = cmsg_align(cmsg_len)?;
        let consumed = space.min(remaining);
        self.hdr = UserPtr::from(self.hdr.as_ptr() as usize + consumed);
        *self.len += consumed;
        Ok(true)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/cmsg.rs");
