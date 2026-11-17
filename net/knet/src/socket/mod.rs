// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common socket wrapper types and poll helpers.
pub(crate) mod file;
pub(crate) mod general;
pub mod options;
pub(crate) mod state;

#[cfg(unittest)]
unittest::path_mod_test!(test_options, "../../ut-tests/socket/test_options.rs");
#[cfg(unittest)]
unittest::path_mod_test!(test_state, "../../ut-tests/socket/test_state.rs");
#[cfg(unittest)]
unittest::path_mod_test!(
    test_socket_identity,
    "../../ut-tests/socket/test_socket_identity.rs"
);

use alloc::{sync::Arc, vec::Vec};
use core::{
    any::Any,
    fmt::{self, Debug},
    net::SocketAddr,
};

use bitflags::bitflags;
use enum_dispatch::enum_dispatch;
use kalloc_types::KBox;
#[cfg(feature = "vsock")]
use kclass::prelude::VsockAddr;
use kcred::Cred;
use kerrno::{KError, KResult, LinuxError};
use kio::prelude::*;

#[cfg(feature = "vsock")]
use crate::vsock::VsockSocket;
use crate::{
    netlink::{NetlinkAddr, NetlinkSocket},
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
    packet::{PacketAddr, PacketSocket},
    raw::RawSocket,
    tcp::TcpSocket,
    udp::UdpSocket,
    unix::{UnixAddr, UnixDomainSocket},
};

#[derive(Clone, Debug)]
pub enum SocketAddrEx {
    Unspecified,
    Ip(SocketAddr),
    Unix(UnixAddr),
    Netlink(NetlinkAddr),
    Packet(PacketAddr),
    #[cfg(feature = "vsock")]
    Vsock(VsockAddr),
}

impl SocketAddrEx {
    pub fn into_ip(self) -> KResult<SocketAddr> {
        match self {
            SocketAddrEx::Unspecified => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Ip(addr) => Ok(addr),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_unix(self) -> KResult<UnixAddr> {
        match self {
            SocketAddrEx::Unspecified => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Unix(addr) => Ok(addr),
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            #[cfg(feature = "vsock")]
            SocketAddrEx::Vsock(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_netlink(self) -> KResult<NetlinkAddr> {
        match self {
            SocketAddrEx::Netlink(addr) => Ok(addr),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    pub fn into_packet(self) -> KResult<PacketAddr> {
        match self {
            SocketAddrEx::Packet(addr) => Ok(addr),
            _ => Err(KError::from(LinuxError::EAFNOSUPPORT)),
        }
    }

    #[cfg(feature = "vsock")]
    pub fn into_vsock(self) -> KResult<VsockAddr> {
        match self {
            SocketAddrEx::Unspecified => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Ip(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Unix(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Netlink(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Packet(_) => Err(KError::from(LinuxError::EAFNOSUPPORT)),
            SocketAddrEx::Vsock(addr) => Ok(addr),
        }
    }
}

bitflags! {
    /// Flags for sending data to a socket.
    ///
    /// See [`SocketOps::send`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SendFlags: u32 {
        /// Do not block for this send operation.
        const DONT_WAIT = 0x01;
        /// Out-of-band data was requested with `MSG_OOB`.
        const OOB = 0x02;
    }
}

impl SendFlags {
    pub(crate) fn nonblocking(self) -> bool {
        self.contains(Self::DONT_WAIT)
    }
}

bitflags! {
    /// Flags for receiving data from a socket.
    ///
    /// See [`SocketOps::recv`].
    #[derive(Default, Debug, Clone, Copy)]
    pub struct RecvFlags: u32 {
        /// Receive data without removing it from the queue.
        const PEEK = 0x01;
        /// For datagram-like sockets, requires [`SocketOps::recv`] to return
        /// the real size of the datagram, even when it is larger than the
        /// buffer.
        const TRUNCATE = 0x02;
        /// Receive a pending asynchronous error instead of data.
        const ERROR_QUEUE = 0x04;
        /// Do not block for this receive operation.
        const DONT_WAIT = 0x08;
        /// Out-of-band data was requested with `MSG_OOB`.
        const OOB = 0x10;
    }
}

impl RecvFlags {
    pub(crate) fn nonblocking(self) -> bool {
        self.contains(Self::DONT_WAIT)
    }
}

/// Type-erased ancillary data carried between the socket layer and its
/// callers.
///
/// An `Arc` is used instead of a `Box` so a control message can be duplicated
/// without knowing its concrete type: on Unix stream sockets a `MSG_PEEK` that
/// reaches a control-message boundary must return the same open file
/// descriptions again (`scm_fp_dup`) without consuming the pending record.
pub type AncillaryData = Arc<dyn Any + Send + Sync>;

/// Options for initiating a socket connection.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectOptions {
    /// Return the protocol-specific nonblocking result when the connection
    /// cannot complete immediately.
    pub nonblocking: bool,
}

/// Options for accepting a connection from a listening socket.
#[derive(Clone, Copy, Debug, Default)]
pub struct AcceptOptions {
    /// Return [`KError::WouldBlock`] when no connection is ready.
    pub nonblocking: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum SocketErrorOrigin {
    Local,
    Icmp,
    Icmp6,
    TxStatus,
}

/// A protocol error stored in a socket error queue.
#[derive(Clone, Debug)]
pub struct SocketErrorInfo {
    pub errno: LinuxError,
    pub origin: SocketErrorOrigin,
    pub error_type: u8,
    pub error_code: u8,
    pub info: u32,
    pub data: u32,
    pub offender: Option<SocketAddr>,
}

/// The byte counts `SIOCOUTQ` and `SIOCINQ` copy out.
///
/// Both fields are byte counts, not message counts: `outq` is the data queued
/// for the peer that it has not consumed yet, `inq` the data the caller can
/// read next.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueueLengths {
    /// Bytes queued towards the peer (Linux `sk_wmem_alloc` /
    /// `unix_outq_len`; the unsent transmit buffer for TCP).
    pub outq: i32,
    /// Bytes ready to be read: the head datagram's length for datagram
    /// sockets, the summed queued bytes for stream and seqpacket sockets.
    pub inq: i32,
}

impl QueueLengths {
    /// Builds both counts from byte totals.
    ///
    /// A queue can in principle outgrow `int`; the ioctl copy-out has no room
    /// for that, so each count saturates instead of wrapping into a value a
    /// caller could mistake for a small or negative queue.
    pub(crate) fn from_bytes(outq: usize, inq: usize) -> Self {
        fn saturate(bytes: usize) -> i32 {
            i32::try_from(bytes).unwrap_or(i32::MAX)
        }
        Self {
            outq: saturate(outq),
            inq: saturate(inq),
        }
    }
}

/// The task identity behind a kernel-attached credentials record.
///
/// The pid is kept as a task-number identity (`struct pid`), not a number:
/// Linux stores one per message (`scm_set_cred`) and resolves it with
/// `pid_vnr` in the receiving task's active PID namespace, so the receiver
/// sees its own namespace's view of the sender. Automatic credentials and an
/// explicit claim naming the sender's TGID carry the process leader; a
/// privileged claim may name any task, including a member thread.
/// `pid` is `None` for kernel peers with a fixed numeric identity (netlink
/// kobject uevents report `pid=0`).
#[derive(Clone)]
pub struct SenderCredentials {
    pub pid: Option<alloc::sync::Arc<kidentity::PidHandle>>,
    pub uid: u32,
    pub gid: u32,
}

impl SenderCredentials {
    /// Identity reported for a kernel-origin netlink skb.
    ///
    /// Linux zeroes `struct sk_buff` through `NETLINK_CB(skb).creds` on
    /// allocation, so ordinary kernel replies and notifications expose numeric
    /// uid/gid 0 and no live task identity. `alloc_uevent_skb` explicitly
    /// installs `GLOBAL_ROOT_UID`/`GLOBAL_ROOT_GID` instead, which projects to
    /// the same zeros in the initial user namespace; both paths leave the PID
    /// field at zero.
    pub(crate) fn kernel() -> Self {
        Self {
            pid: None,
            uid: 0,
            gid: 0,
        }
    }
}

impl core::fmt::Debug for SenderCredentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Only the root projection is printable; the handle's per-namespace
        // number list is internal identity state.
        f.debug_struct("SenderCredentials")
            .field("pid", &self.pid.as_ref().map(|pid| pid.root_nr()))
            .field("uid", &self.uid)
            .field("gid", &self.gid)
            .finish()
    }
}

/// Ancillary data produced by the networking stack itself.
#[derive(Clone)]
pub enum KernelAncillaryData {
    IpError(SocketErrorInfo),
    /// Trusted sender credentials attached by a kernel transport. The inner
    /// identity is opaque to knet: only the producing layer can resolve it.
    Credentials(SenderCredentials),
}

/// Options for sending data to a socket.
///
/// See [`SocketOps::send`].
#[derive(Default)]
pub struct SendOptions {
    pub to: Option<SocketAddrEx>,
    pub flags: SendFlags,
    pub ancillary: Vec<AncillaryData>,
    /// Per-datagram sender identity captured at the socket send boundary.
    ///
    /// Unix transports use it when the receiver enabled `SO_PASSCRED` and for
    /// stream credential records; netlink stores it in the skb-like control
    /// block so every recipient observes the sender of that datagram. Kernel
    /// callers without a current user process may leave it unset, which means
    /// a kernel-origin packet with zeroed credentials.
    pub sender_credentials: Option<SenderCredentials>,
    /// Credentials captured when the socket file was opened.
    ///
    /// Linux authorizes a netlink destination of PID 0 through
    /// `__netlink_ns_capable()`. For an explicit destination the check uses the
    /// sending process only, because `NETLINK_SKB_DST` is set. For an
    /// addressless or connected send it also requires the capability in these
    /// opener credentials. Keeping the snapshot separate from the current send
    /// credentials preserves Linux's two-party check; callers that cannot
    /// supply it fall back to the current process, which preserves direct
    /// kernel API callers.
    ///
    /// Rtnetlink requests require `CAP_NET_ADMIN`; the
    /// `NETLINK_KOBJECT_UEVENT` receive callback requires `CAP_SYS_ADMIN`.
    pub opener_cred: Option<Cred>,
}

impl Debug for SendOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendOptions")
            .field("to", &self.to)
            .field("flags", &self.flags)
            .field("ancillary", &self.ancillary)
            .field("sender_credentials", &self.sender_credentials)
            .field("opener_cred", &self.opener_cred.as_ref().map(|_| ()))
            .finish()
    }
}

/// Options for receiving data from a socket.
///
/// See [`SocketOps::recv`].
#[derive(Default)]
pub struct RecvOptions<'a> {
    pub from: Option<&'a mut SocketAddrEx>,
    pub flags: RecvFlags,
    pub ancillary: Option<&'a mut Vec<AncillaryData>>,
    pub out_flags: Option<&'a mut RecvFlags>,
    /// Reports that control data reached the caller while `ancillary` was
    /// `None` and was therefore dropped: a Unix stream control record that was
    /// reached (a consuming receive drops it) or, on a Unix datagram, the
    /// message's passed file descriptions or the sender credentials exposed by
    /// `SO_PASSCRED`, which are consumed with the message. The caller must
    /// report the loss as `MSG_CTRUNC`, mirroring `__scm_recv_common` and
    /// `scm_detach_fds`. Other transports never set it.
    pub ancillary_discarded: Option<&'a mut bool>,
}
impl Debug for RecvOptions<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecvOptions")
            .field("from", &self.from)
            .field("flags", &self.flags)
            .field("out_flags", &self.out_flags.as_ref().map(|_| ()))
            .field(
                "ancillary_discarded",
                &self.ancillary_discarded.as_deref().map(|_| ()),
            )
            .finish()
    }
}

/// Kind of shutdown operation to perform on a socket.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shutdown {
    Read,
    Write,
    Both,
}
impl Shutdown {
    pub fn has_read(&self) -> bool {
        matches!(self, Shutdown::Read | Shutdown::Both)
    }

    pub fn has_write(&self) -> bool {
        matches!(self, Shutdown::Write | Shutdown::Both)
    }
}

/// Operations that can be performed on a socket.
#[enum_dispatch]
pub trait SocketOps: Configurable {
    /// Binds an unbound socket to the given address and port.
    fn bind(&self, local_addr: SocketAddrEx) -> KResult;
    /// Connects the socket using per-call blocking behavior.
    ///
    /// A nonblocking TCP connection that remains in progress returns
    /// [`KError::InProgress`]. A nonblocking Unix stream connection whose
    /// listener queue is full returns [`KError::WouldBlock`].
    fn connect(&self, remote_addr: SocketAddrEx, options: ConnectOptions) -> KResult;

    /// Starts listening on the bound address and port.
    fn listen(&self, _backlog: usize) -> KResult {
        Err(KError::OperationNotSupported)
    }
    /// Accepts a connection on a listening socket, returning a new socket.
    ///
    /// Requires sleepable, reclaim-safe task context without locks needed by
    /// reclaim, even for nonblocking calls. TCP, Unix and vsock reserve the
    /// returned owner before consuming a pending connection. Owner allocation
    /// failure returns `NoMemory` without dequeuing a connection; transport
    /// errors release the reservation. Nested transport allocations remain
    /// outside this fallible-owner contract.
    fn accept(&self, _options: AcceptOptions) -> KResult<Socket> {
        Err(KError::OperationNotSupported)
    }

    /// Send data to the socket, optionally to a specific address.
    ///
    /// Stream sockets should consume the full input in blocking mode when
    /// progress remains possible, and may return a partial count after a
    /// successful write when the operation would block or fail.
    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize>;
    /// Receive data from the socket.
    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize>;

    /// Get the local endpoint of the socket.
    fn local_addr(&self) -> KResult<SocketAddrEx>;
    /// Get the remote endpoint of the socket.
    fn peer_addr(&self) -> KResult<SocketAddrEx>;

    /// Shutdown the socket, closing the connection.
    fn shutdown(&self, how: Shutdown) -> KResult;

    /// Whether the socket is in the listening state.
    ///
    /// `unix_inq_len` and `tcp_ioctl` refuse `SIOCINQ` for a listening socket
    /// (it holds no data of its own) while `unix_outq_len` still answers
    /// `SIOCOUTQ`, so the ioctl needs to tell the two apart before it asks for
    /// the counts.
    fn is_listening(&self) -> bool {
        false
    }

    /// Reports the byte counts `SIOCOUTQ` and `SIOCINQ` copy out.
    ///
    /// `outq` is what the peer has not consumed yet (Linux's
    /// `sk_wmem_alloc`/`unix_outq_len`, the unsent transmit buffer for TCP),
    /// and `inq` is what the caller can read next: the head datagram's length
    /// for datagram sockets (`first_packet_length`, `unix_inq_len` on
    /// `SOCK_DGRAM`), the summed queued bytes for stream and seqpacket sockets,
    /// and `tcp_inq()` for TCP.
    ///
    /// `None` reports that the transport keeps its receive queue where this
    /// layer cannot inspect it. The ioctl then answers `ENOTTY`, the answer an
    /// unsupported socket ioctl has always given. It must not substitute zero:
    /// glibc 2.43's resolver issues `FIONREAD` on this socket after each query
    /// and abandons the lookup with `EAI_AGAIN` when it reads 0, even though
    /// the reply is already queued.
    fn queue_lengths(&self) -> KResult<Option<QueueLengths>> {
        Ok(None)
    }

    /// Returns the `SO_TYPE` value reported by `getsockopt(SOL_SOCKET,
    /// SO_TYPE)`: the type the socket was created with, which runtime
    /// supervisors (containerd-shim, ttrpc) use to validate inherited
    /// descriptors right after spawn.
    ///
    /// Deliberately has no default: a socket whose type is not reported would
    /// silently answer with `SOCK_STREAM`, so every implementation states it.
    fn socket_type(&self) -> i32;

    /// Returns the `SO_DOMAIN` value reported by `getsockopt(SOL_SOCKET,
    /// SO_DOMAIN)`: the address family the socket was created with, which Linux
    /// keeps in `sk_family`. Has no default for the same reason as
    /// [`Self::socket_type`].
    fn socket_family(&self) -> i32;

    /// Returns the `SO_PROTOCOL` value reported by `getsockopt(SOL_SOCKET,
    /// SO_PROTOCOL)`: the protocol Linux keeps in `sk_protocol`. Families that
    /// never record one report 0, which is what Linux leaves there.
    fn socket_protocol(&self) -> i32;
}

impl<T: SocketOps + ?Sized> SocketOps for KBox<T> {
    fn socket_type(&self) -> i32 {
        (**self).socket_type()
    }

    fn socket_family(&self) -> i32 {
        (**self).socket_family()
    }

    fn socket_protocol(&self) -> i32 {
        (**self).socket_protocol()
    }

    fn bind(&self, local_addr: SocketAddrEx) -> KResult {
        (**self).bind(local_addr)
    }

    fn connect(&self, remote_addr: SocketAddrEx, options: ConnectOptions) -> KResult {
        (**self).connect(remote_addr, options)
    }

    fn listen(&self, backlog: usize) -> KResult {
        (**self).listen(backlog)
    }

    fn accept(&self, options: AcceptOptions) -> KResult<Socket> {
        (**self).accept(options)
    }

    fn send(&self, src: impl Read + IoBuf, options: SendOptions) -> KResult<usize> {
        (**self).send(src, options)
    }

    fn recv(&self, dst: impl Write + IoBufMut, options: RecvOptions<'_>) -> KResult<usize> {
        (**self).recv(dst, options)
    }

    fn local_addr(&self) -> KResult<SocketAddrEx> {
        (**self).local_addr()
    }

    fn peer_addr(&self) -> KResult<SocketAddrEx> {
        (**self).peer_addr()
    }

    fn shutdown(&self, how: Shutdown) -> KResult {
        (**self).shutdown(how)
    }

    fn queue_lengths(&self) -> KResult<Option<QueueLengths>> {
        (**self).queue_lengths()
    }

    fn is_listening(&self) -> bool {
        (**self).is_listening()
    }
}

/// Network socket abstraction.
#[enum_dispatch(Configurable, SocketOps)]
pub enum Socket {
    Udp(KBox<UdpSocket>),
    Tcp(KBox<TcpSocket>),
    Raw(KBox<RawSocket>),
    Unix(KBox<UnixDomainSocket>),
    Netlink(KBox<NetlinkSocket>),
    Packet(KBox<PacketSocket>),
    #[cfg(feature = "vsock")]
    Vsock(KBox<VsockSocket>),
}

impl Socket {
    /// Binds using the credential active at the syscall boundary.
    ///
    /// Netlink group subscriptions can require administrative privilege, so
    /// the bind must observe the caller that performed the syscall rather than
    /// ambient state sampled later. Other families have no credential-sensitive
    /// bind rules and use their normal implementation.
    pub fn bind_with_cred(&self, local_addr: SocketAddrEx, cred: &Cred) -> KResult {
        match self {
            Self::Netlink(socket) => socket.bind_with_cred(local_addr, cred),
            _ => <Self as SocketOps>::bind(self, local_addr),
        }
    }

    /// Whether this socket is an `AF_NETLINK` socket.
    ///
    /// `SOL_NETLINK` options are meaningful only on one; Linux's
    /// `sock_setsockopt` answers EINVAL for an option a family does not
    /// implement, and that decision precedes any privilege check.
    pub fn is_netlink(&self) -> bool {
        matches!(self, Self::Netlink(_))
    }

    /// Whether `NETLINK_ADD_MEMBERSHIP` is admitted without CAP_NET_ADMIN.
    ///
    /// `netlink_setsockopt` gates both membership commands on
    /// `netlink_allowed(sock, NL_CFG_F_NONROOT_RECV)`, which passes when the
    /// protocol's table carries that flag *or* the caller has CAP_NET_ADMIN.
    /// Every netlink protocol this kernel implements sets it: `NETLINK_ROUTE`
    /// (`rtnetlink_net_init`) and `NETLINK_KOBJECT_UEVENT`
    /// (`uevent_net_init`) both do, so unprivileged udev-style subscribers are
    /// normal. The capability therefore only matters for a protocol that does
    /// not, and none is implemented here; the check stays so adding one cannot
    /// silently grant subscription. A non-netlink socket answers `false`; its
    /// own option dispatcher rejects the command before privilege matters.
    pub fn allows_unprivileged_group_subscribe(&self) -> bool {
        match self {
            // Linux's `NL_CFG_F_NONROOT_RECV` is a per-protocol table flag;
            // the netlink layer owns that table and answers from it, so the
            // policy lives in one place.
            Self::Netlink(socket) => socket.allows_unprivileged_group_subscribe(),
            _ => false,
        }
    }

    /// Connects using the credential active at the syscall boundary.
    ///
    /// Netlink connect authorization depends on the caller's capability set;
    /// other protocols accept the same address and options as
    /// [`SocketOps::connect`].
    pub fn connect_with_cred(
        &self,
        remote_addr: SocketAddrEx,
        options: ConnectOptions,
        cred: &Cred,
    ) -> KResult {
        match self {
            Self::Netlink(socket) => socket.connect_with_cred(remote_addr, cred),
            _ => <Self as SocketOps>::connect(self, remote_addr, options),
        }
    }

    /// Sends data using the sending task's credential.
    ///
    /// Netlink authorization depends on the process performing each send, not
    /// on the process that opened the socket. Other protocols use the normal
    /// socket path.
    ///
    /// # Errors
    ///
    /// Returns the protocol-specific send error, including authorization,
    /// address, queue-capacity, and nonblocking failures.
    pub fn send_with_cred(
        &self,
        src: impl Read + IoBuf,
        options: SendOptions,
        sender_cred: &Cred,
    ) -> KResult<usize> {
        match self {
            Self::Netlink(socket) => socket.send_with_cred(src, options, sender_cred),
            _ => <Self as SocketOps>::send(self, src, options),
        }
    }
}
