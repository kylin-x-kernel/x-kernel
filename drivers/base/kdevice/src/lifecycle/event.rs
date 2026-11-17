// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device lifecycle events and notification mechanism.

use alloc::sync::Arc;

use driver_base::DeviceKind;

use crate::DeviceId;

/// A device lifecycle event emitted by the device management layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceEvent {
    /// A live device object was published into the device index.
    Published {
        /// Device that was published.
        id: DeviceId,
    },
    /// At least one driver matched this device and probing is proceeding.
    Matched {
        /// Device that was matched.
        id: DeviceId,
    },
    /// A device was successfully bound to a driver.
    Bound {
        /// Device that was bound.
        id: DeviceId,
        /// Device kind reported by the driver.
        kind: DeviceKind,
    },
    /// A device was activated and is ready for subsystem consumption.
    Activated {
        /// Device that was activated.
        id: DeviceId,
        /// Device kind reported by the driver.
        kind: DeviceKind,
    },
    /// A device was removed (hot-unplug or driver unbind).
    Removed {
        /// Device that was removed.
        id: DeviceId,
    },
    /// One binding instance is terminal and has been (or is being) retired.
    ///
    /// This is instance-scoped, not registration-scoped: the same `DeviceId`
    /// may emit it once per failed candidate. An unbind or unregister emits
    /// `Removed` instead. Consumers holding handles captured from the old
    /// binding must revoke them; the stable object itself may still be
    /// registered and may later bind again.
    BindingRetired {
        /// Device whose binding instance retired.
        id: DeviceId,
    },
}

/// Stable discriminator for lifecycle event subscriptions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceEventKind {
    /// Subscription bucket for [`DeviceEvent::Published`].
    Published,
    /// Subscription bucket for [`DeviceEvent::Matched`].
    Matched,
    /// Subscription bucket for [`DeviceEvent::Bound`].
    Bound,
    /// Subscription bucket for [`DeviceEvent::Activated`].
    Activated,
    /// Subscription bucket for [`DeviceEvent::Removed`].
    Removed,
    /// Subscription bucket for [`DeviceEvent::BindingRetired`].
    BindingRetired,
}

impl DeviceEventKind {
    /// Number of distinct lifecycle event kinds.
    pub(crate) const COUNT: usize = 6;

    /// Dense bucket index for this kind, in `0..COUNT`.
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Published => 0,
            Self::Matched => 1,
            Self::Bound => 2,
            Self::Activated => 3,
            Self::Removed => 4,
            Self::BindingRetired => 5,
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
            Self::BindingRetired { .. } => DeviceEventKind::BindingRetired,
        }
    }
}

pub(crate) type DeviceEventCallback = Arc<dyn Fn(DeviceEvent) + Send + Sync>;
