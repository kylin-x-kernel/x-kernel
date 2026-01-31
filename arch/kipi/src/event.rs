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

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_event {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::{Callback, MulticastCallback};

    #[def_test]
    fn test_callback_executes() {
        static HIT: AtomicUsize = AtomicUsize::new(0);
        let cb = Callback::new(|| {
            HIT.fetch_add(1, Ordering::SeqCst);
        });
        cb.call();
        assert_eq!(HIT.load(Ordering::SeqCst), 1);
    }

    #[def_test]
    fn test_multicast_clone_and_call() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let cb = MulticastCallback::new(|| {
            COUNT.fetch_add(1, Ordering::SeqCst);
        });
        cb.clone().call();
        cb.call();
        assert_eq!(COUNT.load(Ordering::SeqCst), 2);
    }

    #[def_test]
    fn test_unicast_from_multicast() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let mc = MulticastCallback::new(|| {
            COUNT.fetch_add(1, Ordering::SeqCst);
        });
        let uc = mc.into_unicast();
        uc.call();
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);
    }
}
