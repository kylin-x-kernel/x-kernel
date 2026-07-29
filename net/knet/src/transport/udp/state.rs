// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! UDP socket state visible above the packet backend.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};

use ::core::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
    task::Waker,
};
use kerrno::{KError, KResult, LinuxError};
use kpoll::{IoEvents, Pollable};
use ksync::{Mutex, RwLock};

use super::wait::UdpSocketWaiters;
use crate::{
    Shutdown, SocketErrorInfo,
    general::GeneralOptions,
    ip::{IpAddress, IpEndpoint},
    options::{Configurable, GetSocketOption, OptionHandled, SetSocketOption},
};

/// Internet endpoint used by the current transport backend boundary.
pub(crate) type InetEndpoint = IpEndpoint;

/// UDP socket lifecycle tracked by the socket layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UdpSocketLifecycle {
    Init,
    Bound,
    Connected,
    Closed,
}

/// A protocol error queued on a socket.
#[derive(Clone, Debug)]
pub(crate) struct UdpSocketQueuedError {
    pub(crate) payload: Vec<u8>,
    pub(crate) addr: SocketAddr,
    pub(crate) ancillary: SocketErrorInfo,
}

/// UDP socket state shared by the UDP socket wrapper and error delivery path.
pub(crate) struct UdpSocketState {
    lifecycle: RwLock<UdpSocketLifecycle>,
    local_endpoint: RwLock<Option<InetEndpoint>>,
    peer_endpoint: RwLock<Option<(InetEndpoint, IpAddress)>>,
    read_shutdown: AtomicBool,
    write_shutdown: AtomicBool,
    recv_err: AtomicBool,
    socket_error: AtomicI32,
    error_queue: Mutex<VecDeque<UdpSocketQueuedError>>,
    waiters: UdpSocketWaiters,
    options: GeneralOptions,
}

impl UdpSocketState {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            lifecycle: RwLock::new(UdpSocketLifecycle::Init),
            local_endpoint: RwLock::new(None),
            peer_endpoint: RwLock::new(None),
            read_shutdown: AtomicBool::new(false),
            write_shutdown: AtomicBool::new(false),
            recv_err: AtomicBool::new(false),
            socket_error: AtomicI32::new(0),
            error_queue: Mutex::new(VecDeque::new()),
            waiters: UdpSocketWaiters::new(),
            options: GeneralOptions::new(),
        })
    }

    pub(crate) fn lifecycle(&self) -> UdpSocketLifecycle {
        *self.lifecycle.read()
    }

    fn set_lifecycle(&self, state: UdpSocketLifecycle) {
        *self.lifecycle.write() = state;
    }

    pub(crate) fn local_endpoint(&self) -> Option<InetEndpoint> {
        *self.local_endpoint.read()
    }

    pub(crate) fn set_local_endpoint(&self, endpoint: Option<InetEndpoint>) {
        *self.local_endpoint.write() = endpoint;
        self.set_lifecycle(if endpoint.is_some() {
            UdpSocketLifecycle::Bound
        } else {
            UdpSocketLifecycle::Init
        });
    }

    pub(crate) fn peer_endpoint(&self) -> Option<(InetEndpoint, IpAddress)> {
        *self.peer_endpoint.read()
    }

    pub(crate) fn set_peer_endpoint(&self, endpoint: Option<(InetEndpoint, IpAddress)>) {
        *self.peer_endpoint.write() = endpoint;
        self.set_lifecycle(
            match (endpoint.is_some(), self.local_endpoint().is_some()) {
                (true, _) => UdpSocketLifecycle::Connected,
                (false, true) => UdpSocketLifecycle::Bound,
                (false, false) => UdpSocketLifecycle::Init,
            },
        );
    }

    pub(crate) fn set_recv_err(&self, enabled: bool) {
        self.recv_err.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.clear_error_queue();
        }
    }

    pub(crate) fn recv_err_enabled(&self) -> bool {
        self.recv_err.load(Ordering::Relaxed)
    }

    pub(crate) fn enqueue_error(&self, error: UdpSocketQueuedError) {
        if !self.recv_err_enabled() {
            return;
        }

        let mut queue = self.error_queue.lock();
        const MAX_SOCKET_ERROR_QUEUE: usize = 32;
        if queue.len() >= MAX_SOCKET_ERROR_QUEUE {
            queue.pop_front();
        }
        queue.push_back(error);
        self.refresh_socket_error(&queue);
        drop(queue);
        self.waiters.wake_error();
        self.waiters.wake_read();
    }

    pub(crate) fn has_pending_error(&self) -> bool {
        !self.error_queue.lock().is_empty()
    }

    pub(crate) fn has_socket_error(&self) -> bool {
        self.socket_error.load(Ordering::Acquire) != 0
    }

    pub(crate) fn peek_error(&self) -> Option<UdpSocketQueuedError> {
        self.error_queue.lock().front().cloned()
    }

    pub(crate) fn pop_error(&self) -> Option<UdpSocketQueuedError> {
        let mut queue = self.error_queue.lock();
        let error = queue.pop_front();
        self.refresh_socket_error(&queue);
        error
    }

    pub(crate) fn clear_error_queue(&self) {
        let mut queue = self.error_queue.lock();
        queue.clear();
        self.refresh_socket_error(&queue);
    }

    pub(crate) fn consume_socket_error(&self) -> i32 {
        self.socket_error.swap(0, Ordering::AcqRel)
    }

    pub(crate) fn record_socket_error(&self, errno: LinuxError) {
        self.socket_error.store(errno.into_raw(), Ordering::Release);
        self.waiters.wake_error();
        self.waiters.wake_read();
    }

    pub(crate) fn take_socket_error(&self) -> Option<KError> {
        let errno = self.socket_error.swap(0, Ordering::AcqRel);
        if errno == 0 {
            None
        } else {
            Some(KError::from(LinuxError::new(errno)))
        }
    }

    pub(crate) fn shutdown(&self, how: Shutdown) {
        if how.has_read() {
            self.read_shutdown.store(true, Ordering::Release);
            self.waiters.wake_hup();
            self.waiters.wake_read();
        }
        if how.has_write() {
            self.write_shutdown.store(true, Ordering::Release);
            self.waiters.wake_write();
        }
        if how == Shutdown::Both {
            self.set_lifecycle(UdpSocketLifecycle::Closed);
        }
    }

    pub(crate) fn is_read_shutdown(&self) -> bool {
        self.read_shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn is_write_shutdown(&self) -> bool {
        self.write_shutdown.load(Ordering::Acquire)
    }

    pub(crate) fn readiness(&self, backend_events: IoEvents) -> IoEvents {
        self.waiters.readiness(
            backend_events,
            self.lifecycle(),
            self.is_read_shutdown(),
            self.is_write_shutdown(),
            self.has_socket_error() || self.has_pending_error(),
        )
    }

    pub(crate) fn register_waiter(&self, waker: &Waker, events: IoEvents) {
        self.waiters.register(waker, events);
    }

    pub(crate) fn wake_read(&self) {
        self.waiters.wake_read();
    }

    pub(crate) fn reuse_address(&self) -> bool {
        self.options.reuse_address()
    }

    pub(crate) fn set_device_mask(&self, mask: u32) {
        self.options.set_device_mask(mask);
    }

    pub(crate) fn register_rx_waker(&self, waker: &Waker) {
        self.options.register_rx_waker(waker);
    }

    pub(crate) fn register_tx_waker(&self, waker: &Waker) {
        self.options.register_tx_waker(waker);
    }

    pub(crate) fn send_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        nonblocking: bool,
        f: F,
    ) -> KResult<T> {
        self.options
            .send_poller_with_nonblocking(pollable, nonblocking, f)
    }

    pub(crate) fn recv_poller_with_nonblocking<P: Pollable, F: FnMut() -> KResult<T>, T>(
        &self,
        pollable: &P,
        nonblocking: bool,
        f: F,
    ) -> KResult<T> {
        self.options
            .recv_poller_with_nonblocking(pollable, nonblocking, f)
    }

    fn refresh_socket_error(&self, queue: &VecDeque<UdpSocketQueuedError>) {
        let errno = queue
            .front()
            .map(|error| error.ancillary.errno.into_raw())
            .unwrap_or(0);
        self.socket_error.store(errno, Ordering::Release);
    }
}

impl Configurable for UdpSocketState {
    fn get_option_inner(&self, option: &mut GetSocketOption) -> KResult<OptionHandled> {
        if let GetSocketOption::Error(error) = option {
            **error = self.consume_socket_error();
            return Ok(OptionHandled::Yes);
        }
        self.options.get_option_inner(option)
    }

    fn set_option_inner(&self, option: SetSocketOption) -> KResult<OptionHandled> {
        self.options.set_option_inner(option)
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use ::core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use kerrno::LinuxError;
    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;
    use crate::{SocketErrorOrigin, ip::IpEndpoint};

    fn endpoint(addr: Ipv4Addr, port: u16) -> IpEndpoint {
        SocketAddrV4::new(addr, port).into()
    }

    fn queued_error(errno: LinuxError, payload_byte: u8) -> UdpSocketQueuedError {
        UdpSocketQueuedError {
            payload: vec![payload_byte],
            addr: SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 1234)),
            ancillary: SocketErrorInfo {
                errno,
                origin: SocketErrorOrigin::Icmp,
                error_type: 3,
                error_code: 3,
                info: 0,
                data: 0,
                offender: None,
            },
        }
    }

    #[def_test]
    fn test_state_endpoint_state_transitions() {
        let state = UdpSocketState::new();
        let local = endpoint(Ipv4Addr::new(10, 0, 0, 2), 8080);
        let peer = endpoint(Ipv4Addr::new(192, 0, 2, 1), 5353);

        assert_eq!(state.lifecycle(), UdpSocketLifecycle::Init);
        state.set_local_endpoint(Some(local));
        assert_eq!(state.local_endpoint(), Some(local));
        assert_eq!(state.lifecycle(), UdpSocketLifecycle::Bound);

        state.set_peer_endpoint(Some((
            peer,
            IpAddress::Ipv4(Ipv4Addr::new(10, 0, 0, 2).into()),
        )));
        assert_eq!(state.peer_endpoint().map(|it| it.0), Some(peer));
        assert_eq!(state.lifecycle(), UdpSocketLifecycle::Connected);

        state.set_peer_endpoint(None);
        assert_eq!(state.peer_endpoint(), None);
        assert_eq!(state.lifecycle(), UdpSocketLifecycle::Bound);
    }

    #[def_test]
    fn test_state_error_queue_respects_recv_err_option() {
        let state = UdpSocketState::new();

        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));
        assert!(!state.has_pending_error());

        state.set_recv_err(true);
        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 2));
        assert!(state.has_pending_error());
        assert_eq!(
            state.consume_socket_error(),
            LinuxError::ECONNREFUSED.into_raw()
        );
    }

    #[def_test]
    fn test_state_recorded_error_is_read_once() {
        let state = UdpSocketState::new();

        state.record_socket_error(LinuxError::ECONNREFUSED);

        assert_eq!(
            state.take_socket_error(),
            Some(KError::from(LinuxError::ECONNREFUSED))
        );
        assert_eq!(state.take_socket_error(), None);
    }

    #[def_test]
    fn test_state_readiness_merges_shutdown_and_error_state() {
        let state = UdpSocketState::new();

        state.set_recv_err(true);
        state.enqueue_error(queued_error(LinuxError::ECONNREFUSED, 1));
        let events = state.readiness(IoEvents::IN | IoEvents::OUT);
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::OUT));
        assert!(events.contains(IoEvents::ERR));

        state.shutdown(Shutdown::Both);
        let events = state.readiness(IoEvents::IN | IoEvents::OUT);
        assert!(events.contains(IoEvents::HUP));
        assert!(events.contains(IoEvents::RDHUP));
        assert!(!events.contains(IoEvents::OUT));
    }
}
