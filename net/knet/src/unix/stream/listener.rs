// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unix stream listener queue and admission state.

use alloc::{collections::VecDeque, sync::Arc};

use event_listener::{Event, listener};
use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use ksync::Mutex;
use ktask::future::{block_on, interruptible, timeout_at};
use ktime_types::TimeSpan;

use super::channel::{Channel, StreamEndpoint};
use crate::{consts::LISTEN_QUEUE_SIZE, unix::UnixAddr};

pub(super) struct ConnRequest {
    pub(super) channel: Channel,
    pub(super) addr: UnixAddr,
    pub(super) pid: u32,
}

#[derive(Default)]
struct ListenerState {
    pending: VecDeque<ConnRequest>,
    reserved_slots: usize,
    max_pending: usize,
    is_listening: bool,
    is_receive_shutdown: bool,
}

pub(super) struct ListenerQueue {
    state: Mutex<ListenerState>,
    request_available: Event,
    capacity_available: Event,
    endpoint: Arc<StreamEndpoint>,
}

impl ListenerQueue {
    pub(super) fn new(endpoint: Arc<StreamEndpoint>) -> Self {
        Self {
            state: Mutex::new(ListenerState::default()),
            request_available: Event::new(),
            capacity_available: Event::new(),
            endpoint,
        }
    }

    pub(super) fn configure_listen(&self, backlog: usize) -> (bool, bool) {
        // Linux 6.8 `unix_recvq_full_lockless` tests whether the existing
        // receive queue is already greater than the configured backlog.
        let max_pending = backlog.saturating_add(1).clamp(1, LISTEN_QUEUE_SIZE);
        {
            let mut state = self.state.lock();
            let became_listening = !state.is_listening;
            let capacity_increased = max_pending > state.max_pending;
            state.is_listening = true;
            state.max_pending = max_pending;
            (became_listening, capacity_increased)
        }
    }

    pub(super) fn notify_listen_change(&self, became_listening: bool, capacity_increased: bool) {
        if capacity_increased {
            self.capacity_available.notify(usize::MAX);
        }
        if became_listening {
            self.endpoint.polls.wake_state_change();
        }
    }

    pub(super) fn is_listening(&self) -> bool {
        self.state.lock().is_listening
    }

    pub(super) fn poll_state(&self) -> (bool, bool) {
        let state = self.state.lock();
        (state.is_listening, !state.pending.is_empty())
    }

    pub(super) fn reserve(
        self: &Arc<Self>,
        nonblocking: bool,
        send_timeout: Option<TimeSpan>,
    ) -> KResult<ListenerReservation> {
        // Linux 6.8 `unix_stream_connect` waits with
        // `unix_wait_for_peer(other, sock_sndtimeo(sk, noblock))`. The remaining
        // timeout is consumed across restarts; expiry retries once and returns
        // `EAGAIN`. `send_timeout == None` is infinite, matching a zero
        // `SO_SNDTIMEO` / default `sk_sndtimeo`.
        let deadline = send_timeout.and_then(|duration| monotonic_time().checked_add(duration));
        let mut is_send_timeout_expired = false;
        loop {
            listener!(self.capacity_available => capacity_available);
            {
                let mut state = self.state.lock();
                if !state.is_listening || state.is_receive_shutdown {
                    return Err(KError::ConnectionRefused);
                }
                let used_slots = state.pending.len().saturating_add(state.reserved_slots);
                if used_slots < state.max_pending {
                    state.reserved_slots += 1;
                    return Ok(ListenerReservation {
                        listener: self.clone(),
                        is_active: true,
                    });
                }
                if nonblocking || is_send_timeout_expired {
                    return Err(KError::WouldBlock);
                }
            }
            match block_on(timeout_at(deadline, interruptible(capacity_available))) {
                Ok(Ok(())) => {}
                Ok(Err(interrupted)) => return Err(interrupted.into()),
                Err(_elapsed) => is_send_timeout_expired = true,
            }
        }
    }

    pub(super) async fn accept(&self, nonblocking: bool) -> KResult<ConnRequest> {
        loop {
            listener!(self.request_available => request_available);
            let result = {
                let mut state = self.state.lock();
                if !state.is_listening {
                    Some(Err(KError::InvalidInput))
                } else if let Some(request) = state.pending.pop_front() {
                    Some(Ok(request))
                } else if nonblocking {
                    Some(Err(KError::WouldBlock))
                } else if state.is_receive_shutdown {
                    Some(Err(KError::InvalidInput))
                } else {
                    None
                }
            };
            if let Some(result) = result {
                if result.is_ok() {
                    self.capacity_available.notify(1);
                }
                return result;
            }
            request_available.await;
        }
    }

    pub(super) fn mark_receive_shutdown(&self) -> bool {
        let mut state = self.state.lock();
        if state.is_receive_shutdown {
            false
        } else {
            state.is_receive_shutdown = true;
            true
        }
    }

    pub(super) fn notify_shutdown(&self) {
        self.request_available.notify(usize::MAX);
        self.capacity_available.notify(usize::MAX);
    }

    pub(super) fn notify_request_available(&self) {
        self.request_available.notify(1);
        self.endpoint.polls.readable.wake();
    }

    pub(super) fn notify_capacity_available(&self) {
        self.capacity_available.notify(1);
    }

    pub(super) fn close(&self) {
        let pending = {
            let mut state = self.state.lock();
            state.is_listening = false;
            state.is_receive_shutdown = true;
            core::mem::take(&mut state.pending)
        };
        drop(pending);
        self.notify_shutdown();
    }
}

pub(super) struct ListenerReservation {
    listener: Arc<ListenerQueue>,
    is_active: bool,
}

impl ListenerReservation {
    pub(super) fn commit(mut self, request: ConnRequest) -> KResult<()> {
        {
            let mut state = self.listener.state.lock();
            state.reserved_slots -= 1;
            self.is_active = false;
            if !state.is_listening || state.is_receive_shutdown {
                Err(KError::ConnectionRefused)
            } else {
                state.pending.push_back(request);
                Ok(())
            }
        }
    }
}

impl Drop for ListenerReservation {
    fn drop(&mut self) {
        if !self.is_active {
            return;
        }
        self.listener.state.lock().reserved_slots -= 1;
        self.listener.capacity_available.notify(1);
    }
}

#[derive(Clone)]
pub(crate) struct Bind {
    pub(super) listener: Arc<ListenerQueue>,
    pub(super) pid: u32,
}
