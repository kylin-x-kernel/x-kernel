// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Socket wait sets and readiness calculation.

use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet};

use super::state::UdpSocketLifecycle;

/// Read, write, error, and hangup wait sets owned by a socket.
pub(crate) struct UdpSocketWaiters {
    read: PollSet,
    write: PollSet,
    error: PollSet,
    hup: PollSet,
}

impl UdpSocketWaiters {
    pub(crate) fn new() -> Self {
        Self {
            read: PollSet::new(),
            write: PollSet::new(),
            error: PollSet::new(),
            hup: PollSet::new(),
        }
    }

    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::RDNORM | IoEvents::RDBAND) {
            context.register(&self.read)?;
        }
        if events.intersects(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND) {
            context.register(&self.write)?;
        }
        if events.contains(IoEvents::ERR) {
            context.register(&self.error)?;
        }
        if events.intersects(IoEvents::HUP | IoEvents::RDHUP) {
            context.register(&self.hup)?;
        }
        Ok(())
    }

    pub(crate) fn wake_read(&self) {
        self.read.wake();
    }

    pub(crate) fn wake_write(&self) {
        self.write.wake();
    }

    pub(crate) fn wake_error(&self) {
        self.error.wake();
    }

    pub(crate) fn wake_hup(&self) {
        self.hup.wake();
    }

    pub(crate) fn readiness(
        &self,
        backend_events: IoEvents,
        lifecycle: UdpSocketLifecycle,
        read_shutdown: bool,
        write_shutdown: bool,
        has_error: bool,
    ) -> IoEvents {
        let mut events = backend_events;
        if read_shutdown || matches!(lifecycle, UdpSocketLifecycle::Closed) {
            events.insert(IoEvents::RDHUP | IoEvents::HUP);
        }
        if write_shutdown {
            events.remove(IoEvents::OUT | IoEvents::WRNORM | IoEvents::WRBAND);
        }
        events.set(IoEvents::ERR, has_error);
        events
    }
}

unittest::path_mod_test!(tests, "../../../ut-tests/transport/udp/wait/tests.rs");
