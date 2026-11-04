// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device IRQ callback and outcome contracts, independent of host dispatch.

/// Result of servicing an IRQ: unclaimed, handled, or a threaded wake request.
/// Device-specific queue and event routing stays inside the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqEvent {
    handled: bool,
    wake_thread: bool,
}

impl IrqEvent {
    /// The handler claimed the interrupt.
    pub const HANDLED: Self = Self {
        handled: true,
        wake_thread: false,
    };
    /// The handler did not claim the interrupt (shared-line fallback).
    pub const NOT_HANDLED: Self = Self {
        handled: false,
        wake_thread: false,
    };
    /// The handler claimed the interrupt and requests its threaded handler.
    pub const WAKE_THREAD: Self = Self {
        handled: true,
        wake_thread: true,
    };

    /// Whether this handler claimed and serviced the interrupt.
    pub const fn handled(&self) -> bool {
        self.handled
    }

    /// Whether this handler requests its threaded IRQ handler.
    pub const fn wake_thread(&self) -> bool {
        self.wake_thread
    }

    /// Combine handled and threaded-wake outcomes from multiple callbacks.
    pub fn merge(&mut self, other: IrqEvent) {
        self.handled |= other.handled;
        self.wake_thread |= other.wake_thread;
    }
}

impl Default for IrqEvent {
    fn default() -> Self {
        Self::HANDLED
    }
}

/// A device interrupt handler.
///
/// Handlers run in interrupt context: they must not block, must not allocate,
/// and should defer heavy work to a thread. The argument is the OS-visible IRQ
/// number that triggered the handler. Any closure that is
/// `Fn(usize) -> IrqEvent + Send + Sync` implements this trait.
pub trait IrqHandler: Send + Sync {
    /// Service a fired interrupt and report whether it was claimed.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}

/// A sleepable device threaded IRQ handler.
///
/// Thread handlers run in task context after the primary IRQ handler returns
/// [`IrqEvent::WAKE_THREAD`]. They may
/// use sleepable primitives, but must still obey the owning driver's teardown
/// protocol because a supporting provider must drain the IRQ thread before freeing
/// the action. This trait does not imply host support for threaded registration.
pub trait IrqThreadHandler: Send + Sync {
    /// Service deferred interrupt work in task context.
    fn handle(&self, irq: usize) -> IrqEvent;
}

impl<F> IrqThreadHandler for F
where
    F: Fn(usize) -> IrqEvent + Send + Sync,
{
    fn handle(&self, irq: usize) -> IrqEvent {
        self(irq)
    }
}
