// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;

/// A callback function that executes once on the target CPU.
///
/// # Safety
///
/// The callback must be `Send` to safely transfer across CPU boundaries.
pub struct Callback(CallbackKind);

enum CallbackKind {
    Once(KBox<dyn FnMut() + Send>),
    Shared(MulticastCallback),
}

impl Callback {
    /// Prepares a single-use callback with the default KERNEL allocation policy.
    ///
    /// Requires sleepable task context with IRQs/preemption enabled, outside
    /// non-sleepable RCU and without locks needed by reclaim, I/O or filesystem
    /// callbacks. Returns NoMemory if owner allocation fails, dropping captures
    /// without invoking the callback. Execution has its own context requirements.
    pub fn new<F: FnOnce() + Send + 'static>(callback: F) -> KResult<Self> {
        Self::new_with_flags(callback, AllocFlags::KERNEL)
    }

    /// Prepares a callback without sleeping reclaim, including in IRQ context.
    ///
    /// Capture construction and destruction must be valid in the caller's context.
    /// This does not permit NMI or allocator-reentrant use, promise reserve access,
    /// or bound allocation latency. Returns NoMemory without invoking the callback.
    pub fn new_nowait<F: FnOnce() + Send + 'static>(callback: F) -> KResult<Self> {
        Self::new_with_flags(callback, AllocFlags::NOWAIT)
    }

    /// Prepares a callback with an explicit owner allocation policy.
    ///
    /// The context contract of `KBox::new_with_flags` applies. Invalid flags or
    /// allocation failure drop captures without invocation. Permissions apply
    /// only to owner allocation, not capture construction, invocation or destruction.
    pub fn new_with_flags<F: FnOnce() + Send + 'static>(
        callback: F,
        flags: AllocFlags,
    ) -> KResult<Self> {
        let mut callback = Some(callback);
        let owner: KBox<dyn FnMut() + Send> = KBox::new_with_flags(
            move || {
                if let Some(callback) = callback.take() {
                    callback();
                }
            },
            flags,
        )?;
        Ok(Self(CallbackKind::Once(owner)))
    }

    /// Consumes and executes the callback exactly once without owner allocation.
    ///
    /// The body and capture destruction must be valid in the execution context.
    /// IPI handlers cannot sleep; locally executed callbacks inherit caller context.
    pub fn call(self) {
        match self.0 {
            CallbackKind::Once(mut callback) => callback(),
            CallbackKind::Shared(callback) => callback.call(),
        }
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

    /// Converts shared ownership into a single-use dispatch without allocation.
    ///
    /// This does not clone the Arc or add a fallible per-CPU preparation step.
    pub fn into_unicast(self) -> Callback {
        Callback(CallbackKind::Shared(self))
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
pub(crate) struct IpiEvent {
    /// The source CPU ID that sent this IPI event.
    pub src_cpu_id: LogicalCpuId,
    /// The callback function to execute when this IPI event is dispatched.
    pub callback: Callback,
}

unittest::path_mod_test!(pub(crate) mod tests_event, "../ut-tests/event_unittest.rs");
