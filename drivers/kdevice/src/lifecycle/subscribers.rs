// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lifecycle subscriber lists for device-model observers.

use alloc::vec::Vec;

use super::event::{DeviceEventCallback, DeviceEventKind};

/// Subscriber store for device lifecycle notifications.
///
/// Subscribers are bucketed by [`DeviceEventKind`] so dispatch only snapshots
/// the callbacks registered for the kind being emitted, instead of scanning
/// and filtering every subscriber on each event.
pub struct DeviceEventSubscribers {
    by_kind: [Vec<DeviceEventCallback>; DeviceEventKind::COUNT],
}

impl DeviceEventSubscribers {
    /// Create an empty subscriber store.
    pub fn new() -> Self {
        Self {
            by_kind: core::array::from_fn(|_| Vec::new()),
        }
    }

    /// Register a subscriber for one lifecycle event kind.
    pub fn subscribe_kind(&mut self, kind: DeviceEventKind, callback: DeviceEventCallback) {
        self.by_kind[kind.index()].push(callback);
    }

    /// Snapshot the subscribers registered for `kind`. Callbacks must be
    /// invoked outside locks.
    pub fn subscribers_for(&self, kind: DeviceEventKind) -> Vec<DeviceEventCallback> {
        self.by_kind[kind.index()].clone()
    }

    /// Clear subscribers for unit tests.
    #[cfg(unittest)]
    pub fn clear(&mut self) {
        for bucket in &mut self.by_kind {
            bucket.clear();
        }
    }
}

impl Default for DeviceEventSubscribers {
    fn default() -> Self {
        Self::new()
    }
}
