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

    send_timeout_nanos: AtomicU64,
    recv_timeout_nanos: AtomicU64,

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

            send_timeout_nanos: AtomicU64::new(0),
            recv_timeout_nanos: AtomicU64::new(0),

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

    /// Poll for receive readiness and run the provided operation.
    pub(crate) fn recv_poller<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        f: F,
    ) -> KResult<T> {
        self.recv_poller_with_nonblocking(pollable, false, f)
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
                self.set_bound_dev_if(ifindex);
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

unittest::path_mod_test!(tests, "../../ut-tests/socket/general/tests.rs");
