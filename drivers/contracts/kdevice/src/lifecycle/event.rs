// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device lifecycle events and notification mechanism.

use alloc::sync::Arc;

use driver_base::DeviceKind;

use crate::DeviceId;

/// A device lifecycle event emitted by the device management layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEvent {
    /// A live device object was published into the device index.
    Published { id: DeviceId },
    /// At least one driver matched this device and probing is proceeding.
    Matched { id: DeviceId },
    /// A device was successfully bound to a driver.
    Bound { id: DeviceId, kind: DeviceKind },
    /// A device was activated and is ready for subsystem consumption.
    Activated { id: DeviceId, kind: DeviceKind },
    /// A device was removed (hot-unplug or driver unbind).
    Removed { id: DeviceId },
}

/// Stable discriminator for lifecycle event subscriptions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceEventKind {
    Published,
    Matched,
    Bound,
    Activated,
    Removed,
}

impl DeviceEventKind {
    /// Number of distinct lifecycle event kinds.
    pub(crate) const COUNT: usize = 5;

    /// Dense bucket index for this kind, in `0..COUNT`.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Published => 0,
            Self::Matched => 1,
            Self::Bound => 2,
            Self::Activated => 3,
            Self::Removed => 4,
        }
    }
}

impl DeviceEvent {
    /// Return the stable kind of this lifecycle event.
    pub const fn kind(self) -> DeviceEventKind {
        match self {
            Self::Published { .. } => DeviceEventKind::Published,
            Self::Matched { .. } => DeviceEventKind::Matched,
            Self::Bound { .. } => DeviceEventKind::Bound,
            Self::Activated { .. } => DeviceEventKind::Activated,
            Self::Removed { .. } => DeviceEventKind::Removed,
        }
    }
}

pub(crate) type DeviceEventCallback = Arc<dyn Fn(DeviceEvent) + Send + Sync>;
