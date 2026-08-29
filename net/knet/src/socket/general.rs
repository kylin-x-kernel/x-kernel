// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! General socket options and polling helpers.
use core::{
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
    task::Waker,
};

use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, PollContext, PollRegisterError, Pollable};
use ktask::future::{block_on, poll_io, timeout};
use ktime_types::TimeSpan;

use crate::{
    SERVICE,
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
};

/// General options for all sockets.
pub(crate) struct GeneralOptions {
    /// Whether the socket is non-blocking.
    nonblock: AtomicBool,
    /// Whether the socket should reuse the address.
    reuse_address: AtomicBool,
    /// Whether the socket is allowed to send broadcast packets.
    broadcast: AtomicBool,

    send_timeout_nanos: AtomicU64,
    recv_timeout_nanos: AtomicU64,

    device_mask: AtomicU32,
    /// Address-derived RX mask, saved before intersecting `bound_dev_if`.
    addr_device_mask: AtomicU32,
    /// Bound device ifindex; 0 means unbound (`SO_BINDTODEVICE`).
    ///
    /// UDP transmit, receive demux, and RX waker masks honor this value.
    bound_dev_if: AtomicI32,
}
impl Default for GeneralOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl GeneralOptions {
    /// Create a new set of general options with defaults.
    pub fn new() -> Self {
        Self {
            nonblock: AtomicBool::new(false),
            reuse_address: AtomicBool::new(false),
            broadcast: AtomicBool::new(false),

            send_timeout_nanos: AtomicU64::new(0),
            recv_timeout_nanos: AtomicU64::new(0),

            device_mask: AtomicU32::new(0),
            addr_device_mask: AtomicU32::new(0),
            bound_dev_if: AtomicI32::new(0),
        }
    }

    /// Returns whether the socket is non-blocking.
    pub fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    /// Returns whether address reuse is enabled.
    pub fn reuse_address(&self) -> bool {
        self.reuse_address.load(Ordering::Relaxed)
    }

    /// Returns whether broadcast sending is enabled.
    pub fn broadcast(&self) -> bool {
        self.broadcast.load(Ordering::Relaxed)
    }

    /// Returns the bound device ifindex, or 0 if unbound.
    pub fn bound_dev_if(&self) -> i32 {
        self.bound_dev_if.load(Ordering::Relaxed)
    }

    fn bound_device_mask(&self) -> Option<u32> {
        let ifindex = self.bound_dev_if();
        (ifindex > 0).then(|| 1u32.checked_shl((ifindex - 1) as u32).unwrap_or(0))
    }

    /// Intersects `addr_mask` with [`Self::bound_dev_if`] when the socket is
    /// bound to a device.
    pub fn apply_bound_device_mask(&self, addr_mask: u32) {
        self.addr_device_mask.store(addr_mask, Ordering::Release);
        let mask = self
            .bound_device_mask()
            .map_or(addr_mask, |bound| addr_mask & bound);
        self.set_device_mask(mask);
    }

    fn restore_addr_device_mask(&self) {
        let addr_mask = self.addr_device_mask.load(Ordering::Acquire);
        if addr_mask != 0 {
            self.set_device_mask(addr_mask);
        } else if self.device_mask() != 0 {
            self.set_device_mask(u32::MAX);
        }
    }

    #[cfg(unittest)]
    pub fn set_bound_dev_if_for_test(&self, ifindex: i32) {
        self.bound_dev_if.store(ifindex, Ordering::Relaxed);
    }

    /// Returns the configured send timeout.
    pub fn send_timeout(&self) -> Option<TimeSpan> {
        let nanos = self.send_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| TimeSpan::from_nanos(nanos))
    }

    /// Returns the configured receive timeout.
    pub fn recv_timeout(&self) -> Option<TimeSpan> {
        let nanos = self.recv_timeout_nanos.load(Ordering::Relaxed);
        (nanos > 0).then(|| TimeSpan::from_nanos(nanos))
    }

    /// Set the device mask used for receive waker registration.
    pub fn set_device_mask(&self, mask: u32) {
        self.device_mask.store(mask, Ordering::Release);
    }

    /// Return the device mask used for receive wakers.
    pub fn device_mask(&self) -> u32 {
        self.device_mask.load(Ordering::Acquire)
    }

    /// Registers the current poll operation for receive readiness.
    pub fn register_rx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        SERVICE.register_rx_waker(self.device_mask(), context)
    }

    /// Registers for network progress that may free transmit capacity.
    pub fn register_tx_waker(
        &self,
        context: &mut PollContext<'_>,
    ) -> Result<Waker, PollRegisterError> {
        let source_waker = SERVICE.register_rx_waker(u32::MAX, context)?;
        crate::poller::network_poller().register_tx_waker(context)?;
        Ok(source_waker)
    }

    /// Poll for send readiness and run the provided operation.
    pub fn send_poller<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> KResult<T> {
        self.send_poller_with_nonblocking(pollable, false, f)
    }

    /// Poll for send readiness and run the operation with a per-call
    /// nonblocking override.
    pub fn send_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
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

    /// Poll for receive readiness and run the provided operation.
    pub fn recv_poller<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> KResult<T> {
        self.recv_poller_with_nonblocking(pollable, false, f)
    }

    /// Poll for receive readiness and run the operation with a per-call
    /// nonblocking override.
    pub fn recv_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
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
            O::BindToDevice(name) => {
                let ifindex = match name {
                    None => 0,
                    Some(dev_name) => {
                        if dev_name.is_empty() {
                            0
                        } else if SERVICE.is_inited() {
                            SERVICE
                                .link_snapshots()
                                .into_iter()
                                .find(|link| &link.name == dev_name)
                                .map(|link| link.ifindex)
                                .ok_or(KError::from(LinuxError::ENODEV))?
                        } else {
                            return Err(KError::from(LinuxError::ENODEV));
                        }
                    }
                };
                self.bound_dev_if.store(ifindex, Ordering::Relaxed);
                if ifindex > 0 {
                    let stored = self.addr_device_mask.load(Ordering::Acquire);
                    let current = self.device_mask();
                    let addr_mask = if stored != 0 {
                        stored
                    } else if current == 0 {
                        u32::MAX
                    } else {
                        current
                    };
                    self.apply_bound_device_mask(addr_mask);
                } else {
                    self.restore_addr_device_mask();
                }
            }
            O::SendTimeout(timeout) => {
                self.send_timeout_nanos
                    .store(timeout.as_nanos_u64_saturating(), Ordering::Relaxed);
            }
            O::ReceiveTimeout(timeout) => {
                self.recv_timeout_nanos
                    .store(timeout.as_nanos_u64_saturating(), Ordering::Relaxed);
            }
            O::SendBuffer(_) | O::ReceiveBuffer(_) => {
                // TODO(mivik): implement buffer size options
            }
            _ => return Ok(OptionHandled::No),
        }
        Ok(OptionHandled::Yes)
    }
}
