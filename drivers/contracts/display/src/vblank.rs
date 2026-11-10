// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Vertical blanking source of a display device.
//!
//! The DRM layer owns the vblank sequence and the timing state; this contract
//! only describes where a *blank* comes from. A blank means "the display has
//! taken the frame that was pushed before it", which is the completion a page
//! flip and `WAIT_VBLANK` wait for.
//!
//! A driver that can observe that completion reports it through
//! [`VblankSource::subscribe`]. A display without one leaves the capability
//! `None`, and the DRM layer derives blanks from the refresh rate instead.
//! Drivers that report blanks should also publish the refresh rate, so that
//! the derived timeline and the observed one agree.

use alloc::sync::Arc;
use core::fmt;

use driver_base::DriverResult;
use ktime_types::MonotonicInstant;

/// One vertical blanking interval reported by a display device.
///
/// `sequence` counts blanks from the start of the source and wraps like the
/// DRM wire sequence. `timestamp` is the source's estimate of the instant the
/// scanout was latched for that interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VblankTick {
    /// Blank counter, monotonic apart from the wrapping in `u32`.
    pub sequence: u32,
    /// Monotonic instant of this blanking interval.
    pub timestamp: MonotonicInstant,
}

/// Where the blanks of a display come from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VblankOrigin {
    /// The display reports its own scanout intervals, e.g. from a scanout
    /// completion reported by the host. `timestamp` is the instant the device
    /// took the frame.
    Scanout,
    /// The host derives blanks from the refresh rate. `timestamp` is the
    /// planned instant of the interval, not a latch observed on the wire.
    Timer,
}

impl VblankOrigin {
    /// Short name for logs and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scanout => "scanout",
            Self::Timer => "timer",
        }
    }
}

/// Display device capability that reports blanking intervals.
///
/// A driver implements this when the hardware tells it that a frame it pushed
/// has been consumed. A display without it has no scanout timing of its own.
pub trait VblankSource: Send + Sync {
    /// Origin of the blanks this source reports.
    fn origin(&self) -> VblankOrigin;

    /// Starts reporting blanks to `callback`.
    ///
    /// The emitter invokes the callback from its own blank path, which may be
    /// interrupt context: the callback may lock IRQ-safe state and wake
    /// sleepers, but must not sleep, allocate or block.
    ///
    /// Subscribing again with another callback replaces the first: a display
    /// has one blank consumer.
    ///
    /// # Errors
    ///
    /// Returns [`driver_base::DriverError::Unsupported`] when the display
    /// cannot report blanks, and another error when starting the source fails.
    /// The callback is never invoked after an error return.
    fn subscribe(&self, callback: Arc<dyn Fn(VblankTick) + Send + Sync>) -> DriverResult<()>;
}

impl fmt::Debug for dyn VblankSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VblankSource")
            .field("origin", &self.origin())
            .finish_non_exhaustive()
    }
}
