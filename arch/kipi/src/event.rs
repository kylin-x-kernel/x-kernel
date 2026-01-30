// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};

/// A callback function that executes once on the target CPU.
///
/// # Safety
///
/// The callback must be `Send` to safely transfer across CPU boundaries.
pub struct Callback(Box<dyn FnOnce() + Send>);

impl Callback {
    /// Creates a new callback with the given function.
    pub fn new<F: FnOnce() + Send + 'static>(callback: F) -> Self {
        Self(Box::new(callback))
    }

    /// Executes the callback function.
    pub fn call(self) {
        (self.0)()
    }
}

impl<T: FnOnce() + Send + 'static> From<T> for Callback {
    fn from(callback: T) -> Self {
        Self::new(callback)
    }
}

/// A callback that can be cloned and called multiple times (for broadcast).
///
/// # Safety
///
/// The callback must be both `Send` and `Sync` for safe multi-CPU broadcast.
#[derive(Clone)]
pub struct MulticastCallback(Arc<dyn Fn() + Send + Sync>);

impl MulticastCallback {
    /// Creates a new multicast callback.
    pub fn new<F: Fn() + Send + Sync + 'static>(callback: F) -> Self {
        Self(Arc::new(callback))
    }

    /// Converts this multicast callback into a single-use callback.
    pub fn into_unicast(self) -> Callback {
        Callback(Box::new(move || (self.0)()))
    }

    /// Executes the callback function.
    pub fn call(self) {
        (self.0)()
    }
}

impl<T: Fn() + Send + Sync + 'static> From<T> for MulticastCallback {
    fn from(callback: T) -> Self {
        Self::new(callback)
    }
}

/// An IPI event sent from a source CPU to the target CPU.
pub struct IpiEvent {
    /// The source CPU ID that sent this IPI event.
    pub src_cpu_id: usize,
    /// The callback function to execute when this IPI event is dispatched.
    pub callback: Callback,
}
