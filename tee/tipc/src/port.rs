// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, string::String, sync::Arc};
use core::any::Any;

use bitflags::bitflags;
use kerrno::{KError, KResult};
use kpoll::{PollContext, PollRegisterError};
use kspin::SpinNoIrq;
use log::*;
use tipc_handle::HandleWaitState;

use crate::{
    Handle, HandleEventMask, HandleKind, IPC_CHAN_MAX_BUF_SIZE, IPC_CHAN_MAX_BUFS, IpcChan, IpcUuid,
};

bitflags! {
    /// Access policy of a TIPC port.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct IpcPortFlags: u32 {
        /// Permit trusted-application clients.
        const ALLOW_TA_CONNECT = 0x1;
        /// Permit non-secure clients.
        const ALLOW_NS_CONNECT = 0x2;
    }
}

/// Lifecycle state of a TIPC service port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum IpcPortState {
    /// The port has not been published or has been closed.
    Invalid   = 0,
    /// The port is published and accepting connections.
    Listening = 1,
}

/// A TIPC service port and its pending connection queue.
pub struct IpcPort {
    path: String,
    uuid: IpcUuid,
    flags: IpcPortFlags,

    // Queue sizing applied to each accepted channel endpoint.
    num_recv_bufs: usize,
    recv_buf_size: usize,
    inner: SpinNoIrq<IpcPortInner>,

    handle: HandleWaitState,
}

struct IpcPortInner {
    state: IpcPortState,
    // Server endpoints waiting for `accept`.
    pending_list: VecDeque<(Arc<IpcChan>, IpcUuid)>,
}

impl IpcPort {
    pub(crate) fn new(
        uuid: IpcUuid,
        path: String,
        num_recv_bufs: usize,
        recv_buf_size: usize,
        flags: IpcPortFlags,
    ) -> KResult<Arc<Self>> {
        if num_recv_bufs == 0
            || num_recv_bufs > IPC_CHAN_MAX_BUFS
            || recv_buf_size == 0
            || recv_buf_size > IPC_CHAN_MAX_BUF_SIZE
        {
            return Err(KError::InvalidInput);
        }
        Ok(Arc::new(Self {
            path,
            uuid,
            flags,
            num_recv_bufs,
            recv_buf_size,
            inner: SpinNoIrq::new(IpcPortInner {
                state: IpcPortState::Invalid,
                pending_list: VecDeque::new(),
            }),
            handle: HandleWaitState::new(),
        }))
    }

    /// Returns the service path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Returns the server identity associated with this port.
    pub fn uuid(&self) -> IpcUuid {
        self.uuid
    }

    /// Returns the connection policy flags associated with this port.
    pub fn flags(&self) -> IpcPortFlags {
        self.flags
    }

    /// Returns the port state using the names from Trusty's `ipc.h`.
    pub fn state(&self) -> IpcPortState {
        self.inner.lock().state
    }

    /// Returns whether the port is globally published.
    pub fn is_published(&self) -> bool {
        self.state() == IpcPortState::Listening
    }

    /// Marks the port as published and ready to accept connections.
    pub(crate) fn mark_published(&self) -> KResult {
        let mut inner = self.inner.lock();
        if inner.state != IpcPortState::Invalid {
            return Err(KError::AlreadyExists);
        }
        inner.state = IpcPortState::Listening;
        Ok(())
    }

    pub(crate) fn port_attach_client(&self, client: &Arc<IpcChan>) -> KResult {
        if !self.is_published() {
            error!("port {} is not in listening state", self.path());
            return Err(KError::NotFound);
        }
        let client_uuid = client.uuid();

        // Check if connection to specified port is allowed
        self.ipc_port_check_access(client_uuid)?;

        let prepared = IpcChan::prepare_client_attach(
            client,
            self.uuid,
            self.num_recv_bufs,
            self.recv_buf_size,
        )?;
        let mut inner = self.inner.lock();
        if inner.state != IpcPortState::Listening {
            error!("port {} is not in listening state", self.path());
            return Err(KError::NotFound);
        }
        let server = prepared.finish(client)?;
        inner.pending_list.push_back((server, client_uuid));
        // Notify port that there is a pending connection
        self.handle.notify();
        Ok(())
    }

    /// Accepts the oldest pending connection.
    pub fn ipc_port_accept(&self) -> KResult<(Arc<IpcChan>, IpcUuid)> {
        let (server, uuid) = self
            .inner
            .lock()
            .pending_list
            .pop_front()
            .ok_or(KError::WouldBlock)?;
        server.complete_accept()?;
        Ok((server, uuid))
    }

    fn ipc_port_check_access(&self, uuid: IpcUuid) -> KResult<()> {
        let is_ns_client = uuid == IpcUuid::default();
        let allowed = if is_ns_client {
            self.flags.contains(IpcPortFlags::ALLOW_NS_CONNECT)
        } else {
            self.flags.contains(IpcPortFlags::ALLOW_TA_CONNECT)
        };
        if !allowed {
            error!("access denied for port {}", self.path());
            return Err(KError::PermissionDenied);
        }
        Ok(())
    }
}

impl Handle for IpcPort {
    fn kind(&self) -> HandleKind {
        HandleKind::Port
    }

    fn poll(&self, _finalize: bool) -> HandleEventMask {
        let mut event = HandleEventMask::empty();
        let inner = self.inner.lock();
        event.set(HandleEventMask::READY, !inner.pending_list.is_empty());
        event.set(
            HandleEventMask::ERROR,
            inner.state != IpcPortState::Listening,
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
        let (was_published, pending) = {
            let mut inner = self.inner.lock();
            let was_published = inner.state == IpcPortState::Listening;
            inner.state = IpcPortState::Invalid;
            let pending = core::mem::take(&mut inner.pending_list);
            (was_published, pending)
        };
        if was_published {
            crate::registry::unpublish_port(self.path(), self);
        }
        for (channel, _) in pending {
            channel.close();
        }
        self.handle.notify();
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

impl Drop for IpcPort {
    fn drop(&mut self) {
        self.close();
    }
}
