// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    future::poll_fn,
    sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
};

use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError, PollRegistrations};
use kspin::SpinNoIrq;
use ktask::future::{block_on, interruptible};
use log::warn;
use tipc_handle::HandleWaitState;

use crate::{
    Handle, HandleEventMask, HandleKind, IPC_CHAN_AUX_STATE_CONNECTED,
    IPC_CHAN_AUX_STATE_SEND_UNBLOCKED, IPC_CHAN_FLAG_SERVER, IpcMsgInfo, IpcUuid,
    message::{IpcMsgQueue, ReadMsg},
};

/// Lifecycle state of one channel endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IpcChanState {
    /// Server endpoint queued on a port.
    Accepting     = 1,
    /// Client endpoint waiting for the server to accept.
    Connecting    = 2,
    /// Both endpoints are available for message transfer.
    Connected     = 3,
    /// This endpoint or its peer has closed.
    Disconnecting = 4,
}

/// One side of a bidirectional TIPC channel.
pub struct IpcChan {
    state: AtomicU8,
    // Auxiliary events that do not belong in the main lifecycle state.
    aux_state: AtomicU32,
    // Set when the peer failed to send because this endpoint's queue was full.
    peer_send_blocked: AtomicBool,

    peer: SpinNoIrq<Weak<IpcChan>>,

    // This endpoint's receive queue. Senders enqueue into the peer's queue.
    // A client may be created before the destination port exists, so queues
    // are allocated only when the connection attaches to a port.
    msg_queue: SpinNoIrq<Option<IpcMsgQueue>>,

    uuid: IpcUuid,
    handle: HandleWaitState,
    flags: u32,
}

pub(crate) struct PreparedClientAttach {
    server: Arc<IpcChan>,
    client_queue: IpcMsgQueue,
}

impl PreparedClientAttach {
    pub(crate) fn finish(self, client: &Arc<IpcChan>) -> KResult<Arc<IpcChan>> {
        if client.state() != IpcChanState::Connecting {
            return Err(KError::NotConnected);
        }

        let mut peer = client.peer.lock();
        if peer.upgrade().is_some() {
            return Err(KError::BadState);
        }

        let mut msg_queue = client.msg_queue.lock();
        *msg_queue = Some(self.client_queue);
        *peer = Arc::downgrade(&self.server);
        if client.state() != IpcChanState::Connecting {
            *peer = Weak::new();
            *msg_queue = None;
            self.server.close();
            return Err(KError::NotConnected);
        }
        Ok(self.server)
    }
}

impl IpcChan {
    /// Allocates a client endpoint before the destination port necessarily exists.
    pub(crate) fn new_client(uuid: IpcUuid) -> Arc<Self> {
        Arc::new(Self {
            state: AtomicU8::new(IpcChanState::Connecting as u8),
            aux_state: AtomicU32::new(0),
            peer_send_blocked: AtomicBool::new(false),
            peer: SpinNoIrq::new(Weak::new()),
            msg_queue: SpinNoIrq::new(None),
            uuid,
            handle: HandleWaitState::new(),
            flags: 0,
        })
    }

    /// Allocates the server endpoint and queues for a later port attach.
    pub(crate) fn prepare_client_attach(
        client: &Arc<Self>,
        server_uuid: IpcUuid,
        num_recv_bufs: usize,
        recv_buf_size: usize,
    ) -> KResult<PreparedClientAttach> {
        if client.state() != IpcChanState::Connecting || client.peer.lock().upgrade().is_some() {
            return Err(KError::BadState);
        }

        let client_queue = IpcMsgQueue::new(num_recv_bufs, recv_buf_size)?;
        let server_queue = IpcMsgQueue::new(num_recv_bufs, recv_buf_size)?;
        let server = Arc::new(Self {
            state: AtomicU8::new(IpcChanState::Accepting as u8),
            aux_state: AtomicU32::new(0),
            peer_send_blocked: AtomicBool::new(false),
            peer: SpinNoIrq::new(Arc::downgrade(client)),
            msg_queue: SpinNoIrq::new(Some(server_queue)),
            uuid: server_uuid,
            handle: HandleWaitState::new(),
            flags: IPC_CHAN_FLAG_SERVER,
        });
        Ok(PreparedClientAttach {
            server,
            client_queue,
        })
    }

    /// Returns this endpoint's current lifecycle state.
    pub fn state(&self) -> IpcChanState {
        match self.state.load(Ordering::Acquire) {
            1 => IpcChanState::Accepting,
            2 => IpcChanState::Connecting,
            3 => IpcChanState::Connected,
            _ => IpcChanState::Disconnecting,
        }
    }

    /// Returns the identity stored on this endpoint, matching `ipc_chan.uuid`.
    ///
    /// A client endpoint stores the client UUID; a server endpoint stores the
    /// UUID of the port owner.
    pub fn uuid(&self) -> IpcUuid {
        self.uuid
    }

    /// Returns the channel flags defined by Trusty's `ipc.h`.
    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub(crate) fn complete_accept(&self) -> KResult {
        if self
            .state
            .compare_exchange(
                IpcChanState::Accepting as u8,
                IpcChanState::Connected as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err(KError::BadState);
        }
        let peer = self.peer.lock().upgrade().ok_or(KError::NotConnected)?;
        if peer
            .state
            .compare_exchange(
                IpcChanState::Connecting as u8,
                IpcChanState::Connected as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            self.state
                .store(IpcChanState::Disconnecting as u8, Ordering::Release);
            return Err(KError::NotConnected);
        }
        peer.aux_state
            .fetch_or(IPC_CHAN_AUX_STATE_CONNECTED, Ordering::AcqRel);
        peer.handle.notify();
        Ok(())
    }

    /// Sends one complete message into the peer endpoint's receive queue.
    pub fn ipc_send_msg(&self, msg: &[u8]) -> KResult<usize> {
        self.ipc_send_msg_with_handles(msg, &[])
    }

    /// Returns the peer receive slot size that bounds one outgoing message.
    pub(crate) fn peer_recv_buf_size(&self) -> KResult<usize> {
        if self.state() != IpcChanState::Connected {
            return Err(KError::NotConnected);
        }
        let peer = self.peer.lock().upgrade().ok_or(KError::NotConnected)?;
        if peer.state() != IpcChanState::Connected {
            return Err(KError::NotConnected);
        }
        peer.msg_queue
            .lock()
            .as_ref()
            .map(|queue| queue.item_size())
            .ok_or(KError::NotConnected)
    }

    /// Sends one complete message and attached handles into the peer endpoint.
    pub fn ipc_send_msg_with_handles(
        &self,
        msg: &[u8],
        handles: &[Arc<dyn Handle>],
    ) -> KResult<usize> {
        if self.state() != IpcChanState::Connected {
            return Err(KError::NotConnected);
        }
        let peer = self.peer.lock().upgrade().ok_or(KError::NotConnected)?;
        if peer.state() != IpcChanState::Connected {
            return Err(KError::NotConnected);
        }
        // A caller should retire received messages before sending a response.
        // Keeping them in read state consumes receive slots and can race with a
        // peer that immediately sends another message after observing the
        // response. Trusty only warns for this today, so keep the same behavior.
        let has_read_messages = self
            .msg_queue
            .lock()
            .as_ref()
            .ok_or(KError::NotConnected)?
            .has_read_messages();
        if has_read_messages {
            warn!("sending outgoing TIPC message while incoming messages are in read state");
        }
        let result = {
            let mut msg_queue = peer.msg_queue.lock();
            let queue = msg_queue.as_mut().ok_or(KError::NotConnected)?;
            let result = queue.push(msg, handles);
            if matches!(result, Err(KError::WouldBlock)) {
                peer.peer_send_blocked.store(true, Ordering::Release);
            }
            result
        };
        match result {
            Ok(len) => {
                peer.handle.notify();
                Ok(len)
            }
            Err(KError::WouldBlock) => Err(KError::WouldBlock),
            Err(err) => Err(err),
        }
    }

    /// Claims the oldest complete message for reading.
    pub fn ipc_get_msg(&self) -> KResult<IpcMsgInfo> {
        self.msg_queue
            .lock()
            .as_mut()
            .ok_or(KError::NotConnected)?
            .get()
    }

    /// Returns metadata for the oldest complete message without claiming it.
    pub fn ipc_peek_next_filled_msg(&self) -> KResult<IpcMsgInfo> {
        self.msg_queue
            .lock()
            .as_ref()
            .ok_or(KError::NotConnected)?
            .peek_next_filled()
    }

    /// Claims a previously peeked oldest complete message for reading.
    pub fn ipc_get_filled_msg(&self, id: usize) -> KResult {
        self.msg_queue
            .lock()
            .as_mut()
            .ok_or(KError::NotConnected)?
            .get_filled(id)
    }

    /// Reads bytes from a claimed message without releasing its slot.
    pub fn ipc_read_msg(&self, id: usize, offset: usize, out: &mut [u8]) -> KResult<usize> {
        self.msg_queue
            .lock()
            .as_ref()
            .ok_or(KError::NotConnected)?
            .read(id, offset, out)
    }

    /// Returns attached handles from a claimed message without releasing it.
    pub fn ipc_read_msg_handles(
        &self,
        id: usize,
        max_handles: usize,
    ) -> KResult<Vec<Arc<dyn Handle>>> {
        self.msg_queue
            .lock()
            .as_ref()
            .ok_or(KError::NotConnected)?
            .read_handles(id, max_handles)
    }

    /// Copies message bytes and attached handles under one queue lock.
    pub(crate) fn ipc_read_msg_with_handles(
        &self,
        id: usize,
        offset: usize,
        max_len: usize,
        max_handles: usize,
    ) -> KResult<ReadMsg> {
        self.msg_queue
            .lock()
            .as_ref()
            .ok_or(KError::NotConnected)?
            .read_with_handles(id, offset, max_len, max_handles)
    }

    /// Releases a claimed message slot.
    pub fn ipc_put_msg(&self, id: usize) -> KResult {
        let became_writable = self
            .msg_queue
            .lock()
            .as_mut()
            .ok_or(KError::NotConnected)?
            .put(id)?;
        if became_writable
            && self.peer_send_blocked.swap(false, Ordering::AcqRel)
            && let Some(peer) = self.peer.lock().upgrade()
        {
            peer.aux_state
                .fetch_or(IPC_CHAN_AUX_STATE_SEND_UNBLOCKED, Ordering::AcqRel);
            peer.handle.notify();
        }
        Ok(())
    }

    /// Waits synchronously until an asynchronous connect is accepted or closed.
    ///
    /// Returns `Ok(())` once the peer accepts the connection, or
    /// `Err(KError::NotConnected)` if the peer disconnects before accepting.
    ///
    /// # Interruptibility
    ///
    /// If the current task receives a fatal signal (e.g. SIGKILL) while waiting,
    /// this method returns `Err(KError::Interrupted)`. The caller should propagate
    /// this error so the task can proceed with normal exit.
    pub fn wait_connected(&self) -> KResult {
        let mut registrations = PollRegistrations::new();
        loop {
            match self.state() {
                IpcChanState::Connected => return Ok(()),
                IpcChanState::Disconnecting => return Err(KError::NotConnected),
                _ => {
                    // Register through a short-lived poll future to avoid a
                    // separate wait primitive in the core object.
                    let result: Result<(), KError> = block_on(interruptible(poll_fn(|cx| {
                        let mut context = registrations.context(cx);
                        if self
                            .register(&mut context, HandleEventMask::READY | HandleEventMask::HUP)
                            .is_err()
                        {
                            return core::task::Poll::Ready(Err(KError::NoMemory));
                        }
                        if matches!(
                            self.state(),
                            IpcChanState::Connected | IpcChanState::Disconnecting
                        ) {
                            core::task::Poll::Ready(Ok(()))
                        } else {
                            core::task::Poll::Pending
                        }
                    })))
                    .map_err(KError::from)?;
                    result?;
                }
            }
        }
    }
}

impl Handle for IpcChan {
    fn kind(&self) -> HandleKind {
        HandleKind::Channel
    }

    fn poll(&self, finalize: bool) -> HandleEventMask {
        let mut event = HandleEventMask::empty();
        let aux = if finalize {
            self.aux_state.swap(0, Ordering::AcqRel)
        } else {
            self.aux_state.load(Ordering::Acquire)
        };
        event.set(
            HandleEventMask::READY,
            aux & IPC_CHAN_AUX_STATE_CONNECTED != 0,
        );
        event.set(
            HandleEventMask::SEND_UNBLOCKED,
            aux & IPC_CHAN_AUX_STATE_SEND_UNBLOCKED != 0,
        );
        event.set(
            HandleEventMask::MSG,
            self.msg_queue
                .lock()
                .as_ref()
                .is_some_and(|queue| !queue.is_empty()),
        );
        event.set(
            HandleEventMask::HUP,
            self.state() == IpcChanState::Disconnecting,
        );
        event
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        _event_mask: HandleEventMask,
    ) -> Result<(), PollRegisterError> {
        self.handle.register(context)
    }

    fn close(&self) {
        if self
            .state
            .swap(IpcChanState::Disconnecting as u8, Ordering::AcqRel)
            == IpcChanState::Disconnecting as u8
        {
            return;
        }
        self.handle.notify();
        if let Some(peer) = self.peer.lock().upgrade() {
            peer.state
                .store(IpcChanState::Disconnecting as u8, Ordering::Release);
            peer.handle.notify();
        }
    }

    fn set_cookie(&self, cookie: usize) {
        self.handle.set_cookie(cookie);
    }

    fn cookie(&self) -> usize {
        self.handle.cookie()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for IpcChan {
    fn drop(&mut self) {
        self.close();
    }
}
