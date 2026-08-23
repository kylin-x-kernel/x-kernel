// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use bitflags::bitflags;

use super::{WorkQueueAllocError, WorkQueueStartError};

/// Default active-work limit for one logical workqueue binding.
pub const DEFAULT_WORKQUEUE_MAX_ACTIVE: usize = 1024;

bitflags! {
    /// Linux-like workqueue policy flags.
    ///
    /// The names intentionally mirror Linux workqueue flags so callers can
    /// express the policy they need. Current task-context queues only accept
    /// [`WorkQueueFlags::empty()`]; non-empty flags are
    /// rejected instead of being silently downgraded to weaker semantics.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct WorkQueueFlags: u32 {
        /// Per-CPU worker pool policy.
        const PER_CPU = 1 << 0;
        /// Unbound worker pool policy.
        const UNBOUND = 1 << 1;
        /// High-priority worker pool policy.
        const HIGHPRI = 1 << 2;
        /// Forward-progress rescuer policy for memory reclaim paths.
        const MEM_RECLAIM = 1 << 3;
        /// Freezer-aware queue policy.
        const FREEZABLE = 1 << 4;
        /// Power-efficient queue selection policy.
        const POWER_EFFICIENT = 1 << 5;
        /// CPU-intensive work policy.
        const CPU_INTENSIVE = 1 << 6;
        /// Softirq-context bottom-half workqueue policy.
        const BH = 1 << 7;
        /// Ordered queue policy.
        const ORDERED = 1 << 8;
    }
}

/// Requested active-work concurrency limit.
///
/// This is the Linux `max_active` concept, distinct from
/// the shared worker-pool capacity and from the fixed pending-ring capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkQueueMaxActive {
    /// Use the current implementation default.
    Default,
    /// Request an explicit active-work limit.
    Explicit(usize),
}

impl WorkQueueMaxActive {
    /// Creates a max-active request from a Linux-style numeric value.
    ///
    /// `0` means default, matching Linux `alloc_workqueue(..., max_active = 0)`.
    pub const fn new(max_active: usize) -> Self {
        if max_active == 0 {
            Self::Default
        } else {
            Self::Explicit(max_active)
        }
    }

    /// Returns whether this request uses the implementation default.
    pub const fn is_default(self) -> bool {
        matches!(self, Self::Default)
    }

    /// Returns the explicit limit, or `None` when the default is requested.
    pub const fn explicit_limit(self) -> Option<usize> {
        match self {
            Self::Default => None,
            Self::Explicit(max_active) => Some(max_active),
        }
    }
}

/// Attributes for a logical workqueue attached to shared worker pools.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkQueueAttrs {
    flags: WorkQueueFlags,
    max_active: WorkQueueMaxActive,
}

impl WorkQueueAttrs {
    /// Creates default task-context workqueue attributes.
    pub const fn new() -> Self {
        Self {
            flags: WorkQueueFlags::empty(),
            max_active: WorkQueueMaxActive::Default,
        }
    }

    /// Sets Linux-like queue policy flags.
    ///
    /// Current task-context queues reject non-empty flags with
    /// `UnsupportedFlags` because unbound, high-priority, rescuer, freezer,
    /// power, CPU-intensive, BH, and ordered policies require separate
    /// implementation work. Built-in bottom-half runtime instances are exposed
    /// through `system_bh_wq`-style accessors instead of this flag path.
    pub const fn with_flags(mut self, flags: WorkQueueFlags) -> Self {
        self.flags = flags;
        self
    }

    /// Sets a Linux-style `max_active` request.
    ///
    /// `max_active == 0` selects the implementation default. Non-zero values
    /// limit how many work instances from this queue may be active at once.
    pub const fn with_max_active(mut self, max_active: usize) -> Self {
        self.max_active = WorkQueueMaxActive::new(max_active);
        self
    }

    /// Returns requested Linux-like workqueue flags.
    pub const fn flags(self) -> WorkQueueFlags {
        self.flags
    }

    /// Returns the requested active-work limit.
    pub const fn max_active(self) -> WorkQueueMaxActive {
        self.max_active
    }
}

impl Default for WorkQueueAttrs {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkQueueAttrError {
    UnsupportedFlags,
}

#[derive(Clone, Copy)]
pub(crate) struct WorkQueueConfig {
    pub(crate) max_active: usize,
}

impl From<WorkQueueAttrError> for WorkQueueStartError {
    fn from(err: WorkQueueAttrError) -> Self {
        match err {
            WorkQueueAttrError::UnsupportedFlags => Self::UnsupportedFlags,
        }
    }
}

impl From<WorkQueueAttrError> for WorkQueueAllocError {
    fn from(err: WorkQueueAttrError) -> Self {
        match err {
            WorkQueueAttrError::UnsupportedFlags => Self::UnsupportedFlags,
        }
    }
}

pub(crate) fn validate_workqueue_attrs(
    attrs: WorkQueueAttrs,
) -> Result<WorkQueueConfig, WorkQueueAttrError> {
    if !attrs.flags().is_empty() {
        return Err(WorkQueueAttrError::UnsupportedFlags);
    }

    let max_active = attrs
        .max_active()
        .explicit_limit()
        .unwrap_or(DEFAULT_WORKQUEUE_MAX_ACTIVE);
    Ok(WorkQueueConfig { max_active })
}
