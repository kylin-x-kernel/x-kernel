// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! General socket options and polling helpers.
use core::{
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    task::Waker,
};

use kerrno::KResult;
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

    send_timeout_nanos: AtomicU64,
    recv_timeout_nanos: AtomicU64,

    device_mask: AtomicU32,
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

            send_timeout_nanos: AtomicU64::new(0),
            recv_timeout_nanos: AtomicU64::new(0),

            device_mask: AtomicU32::new(0),
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
