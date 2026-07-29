// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Poll readiness and one-shot waiter registration.
//!
//! [`PollSet`] is an IRQ-safe broadcast source. Each logical wait owns a
//! [`PollRegistrations`] value and uses a short-lived [`PollContext`] to
//! register with one or more sources. This ownership model guarantees that
//! timeout, interruption, successful rechecks, and future cancellation all
//! unregister waiters.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

mod events;
mod registration;
mod source;
mod tests;

pub use events::IoEvents;
pub use registration::{PollContext, PollRegisterError, PollRegistration, PollRegistrations};
pub use source::PollSet;
#[cfg(feature = "stats")]
pub use source::WakerStats;

/// A source whose readiness can be queried and whose wake sources can be
/// registered for one logical wait operation.
pub trait Pollable {
    /// Returns a snapshot of currently ready I/O events.
    fn poll(&self) -> IoEvents;

    /// Registers the current logical wait for `events`.
    ///
    /// Implementations register each relevant [`PollSet`] through `context`.
    /// Callers must retain the owning [`PollRegistrations`] across
    /// `core::task::Poll::Pending` and perform a readiness recheck after this
    /// method returns to close the check/register race.
    ///
    /// # Errors
    ///
    /// Returns an error if any required source registration cannot be
    /// established. Registrations already added to `context` remain owned by
    /// its [`PollRegistrations`] and are rolled back when it is cleared or
    /// dropped.
    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError>;
}
