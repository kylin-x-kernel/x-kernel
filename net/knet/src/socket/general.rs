// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! General socket options and polling helpers.
use core::{
    sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollEvent, PollRegisterError, Pollable};
use ktask::future::{block_on, poll_io, timeout};
use ktime_types::TimeSpan;

use crate::{
    SERVICE,
    device::{NetDeviceId, RxDeviceSelection},
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
};

/// The modeled `net.core.wmem_max`, the clamp `SO_SNDBUF` applies to its hint.
///
/// Linux initializes `sysctl_wmem_max` to 4 MiB.
const WMEM_MAX: usize = 4 << 20;
/// The modeled `net.core.rmem_max`, the clamp `SO_RCVBUF` applies.
const RMEM_MAX: usize = 4 << 20;
/// Largest hint `sock_setsockopt` accepts before doubling.
///
/// Linux clamps the hint to `INT_MAX / 2` first so that `val * 2` still fits an
/// `int`; without it a larger request would produce a stored value whose
/// `int` read-back is negative.
const BUFFER_HINT_MAX: usize = i32::MAX as usize / 2;
/// Minimum effective `sk_sndbuf` (`SOCK_MIN_SNDBUF`, twice a minimal skb).
const SNDBUF_MIN: usize = 4_608;
/// Minimum effective `sk_rcvbuf` (`SOCK_MIN_RCVBUF`).
const RCVBUF_MIN: usize = 2_304;

/// General options for all sockets.
pub(crate) struct GeneralOptions {
    /// Whether the socket is non-blocking.
    nonblock: AtomicBool,
    /// Whether the socket should reuse the address.
    reuse_address: AtomicBool,
    /// Whether the socket is allowed to send broadcast packets.
    broadcast: AtomicBool,
    /// Whether received messages include sender credentials.
    pass_credentials: AtomicBool,
    /// Whether received messages include the peer's security label (SO_PASSSEC).
    /// No LSM is modeled, so the flag is tracked but no label is ever attached.
    pass_security: AtomicBool,
    /// Whether received messages may carry SCM_RIGHTS file descriptors
    /// (SO_PASSRIGHTS). Tracked for compatibility; rights delivery is unchanged.
    pass_rights: AtomicBool,
    /// SO_TIMESTAMP / SO_TIMESTAMPNS enablement. The flag is tracked; receive
    /// timestamping ancillary data is not synthesized.
    timestamp: AtomicBool,
    timestampns: AtomicBool,
    /// SO_TIMESTAMPING flags; tracked, no hardware/software timestamp generation.
    timestamping: AtomicI32,
    /// Netlink reporting/behavior toggles; tracked for compatibility only.
    netlink_ext_ack: AtomicBool,
    netlink_get_strict_chk: AtomicBool,
    netlink_pktinfo: AtomicBool,
    ip_pktinfo: AtomicBool,
    ip_recv_ttl: AtomicBool,
    ip_recv_tos: AtomicBool,
    tcp_fastopen: AtomicI32,
    ip_multicast_ttl: AtomicI32,
    ip_recv_fragsize: AtomicBool,
    pass_pidfd: AtomicBool,

    send_timeout_nanos: AtomicU64,
    recv_timeout_nanos: AtomicU64,

    /// Advertised send/receive buffer sizes (SO_SNDBUF/SO_RCVBUF). The data
    /// path uses its own channel capacity; these are the values userspace
    /// reads back, defaulting to Linux's stock socket buffer size.
    send_buffer_bytes: AtomicU64,
    receive_buffer_bytes: AtomicU64,

    /// Bound device ifindex; zero selects all devices for RX polling.
    bound_dev_if: AtomicI32,
    /// Re-registers RX waiters when their device selection becomes stale.
    device_binding_changed: PollEvent,
}
impl Default for GeneralOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl GeneralOptions {
    /// Create a new set of general options with defaults.
    pub(crate) fn new() -> Self {
        Self {
            nonblock: AtomicBool::new(false),
            reuse_address: AtomicBool::new(false),
            broadcast: AtomicBool::new(false),
            pass_credentials: AtomicBool::new(false),
            pass_security: AtomicBool::new(false),
            pass_rights: AtomicBool::new(false),
            timestamp: AtomicBool::new(false),
            timestampns: AtomicBool::new(false),
            timestamping: AtomicI32::new(0),
            netlink_ext_ack: AtomicBool::new(false),
            netlink_get_strict_chk: AtomicBool::new(false),
            netlink_pktinfo: AtomicBool::new(false),
            ip_pktinfo: AtomicBool::new(false),
            ip_recv_ttl: AtomicBool::new(false),
            ip_recv_tos: AtomicBool::new(false),
            tcp_fastopen: AtomicI32::new(0),
            ip_multicast_ttl: AtomicI32::new(1),
            ip_recv_fragsize: AtomicBool::new(false),
            pass_pidfd: AtomicBool::new(false),

            send_timeout_nanos: AtomicU64::new(0),
            recv_timeout_nanos: AtomicU64::new(0),

            // Linux's default SO_SNDBUF/SO_RCVBUF (net.core.wmem_default /
            // rmem_default); callers such as systemd-resolved read these back.
            send_buffer_bytes: AtomicU64::new(212_992),
            receive_buffer_bytes: AtomicU64::new(212_992),

            bound_dev_if: AtomicI32::new(0),
            device_binding_changed: PollEvent::new(),
        }
    }

    /// Returns whether the socket is non-blocking.
    pub(crate) fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    /// Returns whether address reuse is enabled.
    pub(crate) fn reuse_address(&self) -> bool {
        self.reuse_address.load(Ordering::Relaxed)
    }

    /// Returns whether broadcast sending is enabled.
    pub(crate) fn broadcast(&self) -> bool {
        self.broadcast.load(Ordering::Relaxed)
    }

    /// Returns whether credential ancillary messages are enabled.
    pub(crate) fn pass_credentials(&self) -> bool {
        self.pass_credentials.load(Ordering::Relaxed)
    }

    /// Returns the bound device ifindex, or 0 if unbound.
    ///
    /// Lock-free and valid from NetRx. A concurrent update may yield the old
    /// or new ifindex.
    pub(crate) fn bound_dev_if(&self) -> i32 {
        // This scalar does not publish other state to RX/TX readers.
        self.bound_dev_if.load(Ordering::Relaxed)
    }

    fn set_bound_dev_if(&self, ifindex: i32) {
        // Each changed exchange is followed by a notification. PollEvent's
        // release/acquire generation publishes the preceding scalar update;
        // an exchange still awaiting notification will wake registered waiters.
        if self.bound_dev_if.swap(ifindex, Ordering::Relaxed) != ifindex {
            self.device_binding_changed.notify();
        }
    }

    #[cfg(unittest)]
    pub(crate) fn set_bound_dev_if_for_test(&self, ifindex: i32) {
        self.set_bound_dev_if(ifindex);
    }

    /// Resolves a `SO_BINDTODEVICE` name to an ifindex; empty/`None` clears it.
    fn device_ifindex(name: Option<&str>) -> KResult<i32> {
        match name {
            None | Some("") => Ok(0),
            Some(dev_name) => {
                if !SERVICE.is_inited() {
                    return Err(KError::from(LinuxError::ENODEV));
                }
                SERVICE
                    .link_snapshots()
                    .into_iter()
                    .find(|link| link.name == dev_name)
                    .map(|link| link.ifindex)
                    .ok_or(KError::from(LinuxError::ENODEV))
            }
        }
    }

    /// Returns the configured send timeout.
    pub(crate) fn send_timeout(&self) -> Option<TimeSpan> {
        let nanos = self.send_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| TimeSpan::from_nanos(nanos))
    }

    /// Returns the configured receive timeout.
    pub(crate) fn recv_timeout(&self) -> Option<TimeSpan> {
        let nanos = self.recv_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| TimeSpan::from_nanos(nanos))
    }

    /// Returns the RX device selection for `SO_BINDTODEVICE`.
    ///
    /// An unbound socket registers all devices, independently of its address.
    pub(crate) fn rx_device_selection(&self) -> RxDeviceSelection {
        NetDeviceId::new(self.bound_dev_if())
            .map_or(RxDeviceSelection::All, RxDeviceSelection::Interface)
    }

    /// Registers the current poll operation for receive readiness.
    ///
    /// Keeps a configuration-change registration across the wait. Changes
    /// during registration force a recheck; later changes wake the waiter to
    /// rebuild its device registrations. Runs in task context without spinlocks.
    pub(crate) fn register_rx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        self.register_rx_waker_with(context, |selection, context| {
            SERVICE.register_rx_waker(selection, context)
        })
    }

    fn register_rx_waker_with(
        &self,
        context: &mut PollContext<'_>,
        register_devices: impl FnOnce(
            RxDeviceSelection,
            &mut PollContext<'_>,
        ) -> Result<Waker, PollRegisterError>,
    ) -> Result<Waker, PollRegisterError> {
        // Observe the generation before device selection: an intervening commit must
        // force a recheck even if its wake precedes our event registration.
        let generation = self.device_binding_changed.generation();
        let source_waker = register_devices(self.rx_device_selection(), context)?;
        self.device_binding_changed.register(context)?;
        if self.device_binding_changed.has_changed_since(generation) {
            context.wake_by_ref();
        }
        Ok(source_waker)
    }

    /// Registers for network progress that may free transmit capacity.
    pub(crate) fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        SERVICE.register_tx_waker(context)
    }

    /// Poll for send readiness and run the provided operation.
    #[cfg(feature = "vsock")]
    pub(crate) fn send_poller<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> KResult<T> {
        self.send_poller_with_nonblocking(pollable, false, f)
    }

    /// Poll for send readiness and run the operation with a per-call
    /// nonblocking override.
    pub(crate) fn send_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        nonblocking: bool,
        f: F,
    ) -> KResult<T> {
        block_on(timeout(
            self.send_timeout(),
            poll_io(
                pollable,
                IoEvents::OUT,
                self.nonblocking() || nonblocking,
                f,
            ),
        ))?
    }

    /// Poll for receive readiness and run the operation with a per-call
    /// nonblocking override.
    pub(crate) fn recv_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        nonblocking: bool,
        f: F,
    ) -> KResult<T> {
        block_on(timeout(
            self.recv_timeout(),
            poll_io(pollable, IoEvents::IN, self.nonblocking() || nonblocking, f),
        ))?
    }

    /// Applies `sock_setsockopt(SO_SNDBUF)` / `SO_SNDBUFFORCE`.
    ///
    /// `clamp_bytes` is the modeled `net.core.wmem_max` for the unprivileged form and
    /// `None` for the `CAP_NET_ADMIN` form, which skips only that clamp. Both
    /// then bound the hint so `val * 2` fits an `int`, double it, and apply
    /// `SOCK_MIN_SNDBUF`; `SO_SNDBUF` reports that stored value, which is why a
    /// client that sets N reads back 2N.
    fn set_send_buffer(&self, hint_bytes: usize, clamp_bytes: Option<usize>) {
        let requested = clamp_bytes
            .unwrap_or(usize::MAX)
            .min(hint_bytes)
            .min(BUFFER_HINT_MAX);
        let effective = requested.saturating_mul(2).max(SNDBUF_MIN);
        self.send_buffer_bytes
            .store(effective as u64, Ordering::Relaxed);
    }

    /// Applies `sock_setsockopt(SO_RCVBUF)` / `SO_RCVBUFFORCE`.
    ///
    /// Same rule as [`Self::set_send_buffer`] with `rmem_max` and
    /// `SOCK_MIN_RCVBUF` (`__sock_set_rcvbuf`).
    fn set_receive_buffer(&self, hint_bytes: usize, clamp_bytes: Option<usize>) {
        let requested = clamp_bytes
            .unwrap_or(usize::MAX)
            .min(hint_bytes)
            .min(BUFFER_HINT_MAX);
        let effective = requested.saturating_mul(2).max(RCVBUF_MIN);
        self.receive_buffer_bytes
            .store(effective as u64, Ordering::Relaxed);
    }
}
impl Configurable for GeneralOptions {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        use GetSocketOption as O;
        match option {
            O::Error(error) => {
                // TODO(mivik): actual logic
                **error = 0;
            }
            O::NonBlocking(nonblock) => {
                **nonblock = self.nonblocking();
            }
            O::ReuseAddress(reuse) => {
                **reuse = self.reuse_address();
            }
            O::Broadcast(broadcast) => {
                **broadcast = self.broadcast();
            }
            O::PassCredentials(enabled) => {
                **enabled = self.pass_credentials();
            }
            O::PassSecurity(enabled) => {
                **enabled = self.pass_security.load(Ordering::Relaxed);
            }
            O::PassRights(enabled) => {
                **enabled = self.pass_rights.load(Ordering::Relaxed);
            }
            O::Timestamp(enabled) => {
                **enabled = self.timestamp.load(Ordering::Relaxed);
            }
            O::TimestampNs(enabled) => {
                **enabled = self.timestampns.load(Ordering::Relaxed);
            }
            O::Timestamping(flags) => {
                **flags = self.timestamping.load(Ordering::Relaxed) as u32;
            }
            O::NetlinkExtAck(enabled) => {
                **enabled = self.netlink_ext_ack.load(Ordering::Relaxed);
            }
            O::NetlinkGetStrictChk(enabled) => {
                **enabled = self.netlink_get_strict_chk.load(Ordering::Relaxed);
            }
            O::NetlinkPktinfo(enabled) => {
                **enabled = self.netlink_pktinfo.load(Ordering::Relaxed);
            }
            O::IpPktinfo(enabled) => {
                **enabled = self.ip_pktinfo.load(Ordering::Relaxed);
            }
            O::IpRecvTtl(enabled) => {
                **enabled = self.ip_recv_ttl.load(Ordering::Relaxed);
            }
            O::IpRecvTos(enabled) => {
                **enabled = self.ip_recv_tos.load(Ordering::Relaxed);
            }
            O::BindToIfindex(ifindex) => {
                **ifindex = self.bound_dev_if();
            }
            O::TcpFastopen(queue) => {
                **queue = self.tcp_fastopen.load(Ordering::Relaxed) as u32;
            }
            O::AcceptConn(listening) => {
                // Listening state is transport-specific; transports that can
                // listen override this. The shared default is "not listening".
                **listening = false;
            }
            O::IpMulticastTtl(ttl) => {
                **ttl = self.ip_multicast_ttl.load(Ordering::Relaxed) as u32;
            }
            O::IpRecvFragsize(enabled) => {
                **enabled = self.ip_recv_fragsize.load(Ordering::Relaxed);
            }
            O::PassPidfd(enabled) => {
                **enabled = self.pass_pidfd.load(Ordering::Relaxed);
            }
            O::SendBuffer(size) => {
                **size = self.send_buffer_bytes.load(Ordering::Relaxed) as usize;
            }
            O::ReceiveBuffer(size) => {
                **size = self.receive_buffer_bytes.load(Ordering::Relaxed) as usize;
            }
            O::BindToDevice(name) => {
                let ifindex = self.bound_dev_if();
                **name = if ifindex > 0 && SERVICE.is_inited() {
                    SERVICE
                        .link_snapshot_for_ifindex(ifindex)
                        .map(|link| link.name)
                } else {
                    None
                };
            }
            O::SendTimeout(timeout) => {
                **timeout = TimeSpan::from_nanos(self.send_timeout_nanos.load(Ordering::Relaxed));
            }
            O::ReceiveTimeout(timeout) => {
                **timeout = TimeSpan::from_nanos(self.recv_timeout_nanos.load(Ordering::Relaxed));
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> KResult<OptionHandled> {
        use SetSocketOption as O;

        match option {
            O::NonBlocking(nonblock) => {
                self.nonblock.store(*nonblock, Ordering::Relaxed);
            }
            O::ReuseAddress(reuse) => {
                self.reuse_address.store(*reuse, Ordering::Relaxed);
            }
            O::Broadcast(broadcast) => {
                self.broadcast.store(*broadcast, Ordering::Relaxed);
            }
            O::PassCredentials(enabled) => {
                self.pass_credentials.store(*enabled, Ordering::Relaxed);
            }
            O::PassSecurity(enabled) => {
                self.pass_security.store(*enabled, Ordering::Relaxed);
            }
            O::PassRights(enabled) => {
                self.pass_rights.store(*enabled, Ordering::Relaxed);
            }
            O::Timestamp(enabled) => {
                self.timestamp.store(*enabled, Ordering::Relaxed);
            }
            O::TimestampNs(enabled) => {
                self.timestampns.store(*enabled, Ordering::Relaxed);
            }
            O::Timestamping(flags) => {
                self.timestamping.store(*flags as i32, Ordering::Relaxed);
            }
            O::NetlinkExtAck(enabled) => {
                self.netlink_ext_ack.store(*enabled, Ordering::Relaxed);
            }
            O::NetlinkGetStrictChk(enabled) => {
                self.netlink_get_strict_chk
                    .store(*enabled, Ordering::Relaxed);
            }
            O::NetlinkPktinfo(enabled) => {
                self.netlink_pktinfo.store(*enabled, Ordering::Relaxed);
            }
            O::IpPktinfo(enabled) => {
                self.ip_pktinfo.store(*enabled, Ordering::Relaxed);
            }
            O::IpRecvTtl(enabled) => {
                self.ip_recv_ttl.store(*enabled, Ordering::Relaxed);
            }
            O::IpRecvTos(enabled) => {
                self.ip_recv_tos.store(*enabled, Ordering::Relaxed);
            }
            O::BindToIfindex(ifindex) => {
                self.set_bound_dev_if(*ifindex);
            }
            O::TcpFastopen(queue) => {
                self.tcp_fastopen.store(*queue as i32, Ordering::Relaxed);
            }
            O::ReceiveBufferForce(size) => {
                // `SO_RCVBUFFORCE` is the CAP_NET_ADMIN form: it skips the
                // `rmem_max` clamp but still bounds the hint, doubles it and
                // applies `SOCK_MIN_RCVBUF` (`__sock_set_rcvbuf`).
                let requested = (*size).min(BUFFER_HINT_MAX);
                let effective = requested.saturating_mul(2).max(RCVBUF_MIN);
                self.receive_buffer_bytes
                    .store(effective as u64, Ordering::Relaxed);
            }
            O::IpMulticastTtl(ttl) => {
                self.ip_multicast_ttl.store(*ttl as i32, Ordering::Relaxed);
            }
            O::IpRecvFragsize(enabled) => {
                self.ip_recv_fragsize.store(*enabled, Ordering::Relaxed);
            }
            O::PassPidfd(enabled) => {
                self.pass_pidfd.store(*enabled, Ordering::Relaxed);
            }
            O::BindToDevice(name) => {
                self.set_bound_dev_if(Self::device_ifindex(name.as_deref())?);
            }
            O::SendTimeout(timeout) => {
                self.send_timeout_nanos
                    .store(timeout.as_nanos_u64_saturating(), Ordering::Relaxed);
            }
            O::ReceiveTimeout(timeout) => {
                self.recv_timeout_nanos
                    .store(timeout.as_nanos_u64_saturating(), Ordering::Relaxed);
            }
            O::SendBuffer(size) => self.set_send_buffer(*size, Some(WMEM_MAX)),
            O::ReceiveBuffer(size) => self.set_receive_buffer(*size, Some(RMEM_MAX)),
            O::SendBufferForce(size) => self.set_send_buffer(*size, None),
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/socket/general/tests.rs");
