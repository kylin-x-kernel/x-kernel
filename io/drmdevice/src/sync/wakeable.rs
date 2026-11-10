// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! A poll source a fence can wake.
//!
//! Several objects are readable exactly when a fence completes (a dma-buf
//! descriptor, a syncobj eventfd target). They all need the same two things:
//! a [`PollSet`] and a way for a fence callback to reach it. This type is that
//! shared part, so a fence callback names one small object regardless of which
//! descriptor owns the waiters.

use core::sync::atomic::{AtomicBool, Ordering};

use kpoll::{PollContext, PollRegisterError, PollSet};

/// A readiness source that a fence completes.
#[derive(Default)]
pub(crate) struct Wakeable {
    poll: PollSet,
    armed: AtomicBool,
}

impl Wakeable {
    /// An empty source.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Arms one callback for this direction.
    ///
    /// Returns false when a callback is already armed; only one completion
    /// callback per direction is needed for the whole pending fence set.
    pub(crate) fn arm(&self) -> bool {
        !self.armed.swap(true, Ordering::AcqRel)
    }

    /// Wakes every registered waiter.
    ///
    /// Called from the completing fence's callback, possibly in interrupt
    /// context: it only enqueues tasks.
    pub(crate) fn notify(&self) {
        self.poll.wake();
    }

    /// Wakes the waiters of one dma-buf poll direction.
    pub(crate) fn wake_direction(&self, _write: bool) {
        self.armed.store(false, Ordering::Release);
        self.notify();
    }

    /// Registers `context` for readiness.
    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: kpoll::IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.is_empty() {
            context.register(&self.poll)?;
        }
        Ok(())
    }
}
