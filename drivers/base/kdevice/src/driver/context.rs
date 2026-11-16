// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Phase-scoped lifecycle contexts.
//!
//! A context can only be constructed while the caller owns the descriptor
//! binding lock. The private permit ties its lifetime to that write window and
//! the raw-pointer marker makes it neither `Send` nor `Sync`.

use alloc::sync::Arc;
use core::marker::PhantomData;

use kmutex::MutexGuard;

use crate::{
    BindingRef, DeviceObject, DriverResult,
    device::{DataInstallError, DataTakeError},
    driver::DriverData,
};

/// Lifetime proof for one descriptor write window.
pub(crate) struct ContextPermit<'a> {
    _guard: PhantomData<&'a mut ()>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> ContextPermit<'a> {
    pub(crate) fn new(_guard: &'a MutexGuard<'_, ()>) -> Self {
        Self {
            _guard: PhantomData,
            _not_send_sync: PhantomData,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn for_test() -> ContextPermit<'a> {
        ContextPermit {
            _guard: PhantomData,
            _not_send_sync: PhantomData,
        }
    }
}

/// Context for one explicit probe.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<kdevice::ProbeContext<'static>>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<kdevice::ProbeContext<'static>>();
/// ```
///
/// ```compile_fail
/// fn cannot_escape<'a>(ctx: kdevice::ProbeContext<'a>) -> kdevice::ProbeContext<'static> {
///     ctx
/// }
/// ```
///
/// ```no_run
/// fn positive_control(ctx: &kdevice::ProbeContext<'_>) {
///     let _ = ctx.binding().state();
/// }
/// ```
///
/// ```compile_fail
/// let _forged = kdevice::ProbeContext {
///     device: unreachable!(),
///     binding: unreachable!(),
///     _permit: unreachable!(),
///     _not_send_sync: core::marker::PhantomData,
/// };
/// ```
pub struct ProbeContext<'a> {
    device: Arc<DeviceObject>,
    binding: BindingRef,
    _permit: &'a ContextPermit<'a>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> ProbeContext<'a> {
    pub(crate) fn new(
        device: Arc<DeviceObject>,
        binding: BindingRef,
        permit: &'a ContextPermit<'a>,
    ) -> Self {
        Self {
            device,
            binding,
            _permit: permit,
            _not_send_sync: PhantomData,
        }
    }

    /// Stable device object.
    pub fn device(&self) -> &Arc<DeviceObject> {
        &self.device
    }

    /// Exact binding instance owned by this probe.
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    /// Install probe-owned Data into this exact binding.
    pub(crate) fn install_data(&self, data: DriverData) -> Result<(), DataInstallError> {
        self.device.install_data(self.binding.runtime(), data)
    }

    /// Register one cleanup on this exact binding.
    pub fn add_cleanup<F>(&self, cleanup: F) -> DriverResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        self.binding
            .add_cleanup_exact(self.device.as_ref(), cleanup)
    }

    /// Scoped test-only constructor. The permit lives for this call, and a
    /// registered object still needs the real descriptor binding lock.
    #[cfg(unittest)]
    pub fn with_test_context<R>(
        device: Arc<DeviceObject>,
        f: impl FnOnce(&ProbeContext<'_>) -> R,
    ) -> Option<R> {
        let binding = device.binding_ref_for_lifecycle_write()?;
        let permit = ContextPermit::for_test();
        let ctx = ProbeContext::new(device, binding, &permit);
        Some(f(&ctx))
    }
}

/// Context for one remove lifecycle callback.
///
/// ```compile_fail
/// fn cannot_cleanup(ctx: &kdevice::RemoveContext<'_>) {
///     let _ = ctx.add_cleanup(|| {});
/// }
/// ```
pub struct RemoveContext<'a> {
    device: Arc<DeviceObject>,
    binding: BindingRef,
    _permit: &'a ContextPermit<'a>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> RemoveContext<'a> {
    pub(crate) fn new(
        device: Arc<DeviceObject>,
        binding: BindingRef,
        permit: &'a ContextPermit<'a>,
    ) -> Self {
        Self {
            device,
            binding,
            _permit: permit,
            _not_send_sync: PhantomData,
        }
    }

    /// Stable device object.
    pub fn device(&self) -> &Arc<DeviceObject> {
        &self.device
    }

    /// Exact binding instance being removed.
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    /// Take this exact binding's Data for removal.
    pub(crate) fn take_data(&self) -> Result<Option<DriverData>, DataTakeError> {
        self.device.take_data(self.binding.runtime())
    }

    #[cfg(unittest)]
    pub fn with_test_context<R>(
        device: Arc<DeviceObject>,
        f: impl FnOnce(&RemoveContext<'_>) -> R,
    ) -> Option<R> {
        let binding = device.binding_ref_for_removal()?;
        let permit = ContextPermit::for_test();
        let ctx = RemoveContext::new(device, binding, &permit);
        Some(f(&ctx))
    }
}

/// Context reserved for a future PM callback. It is not invoked by kpm yet.
///
/// ```compile_fail
/// fn cannot_cleanup(ctx: &kdevice::PmContext<'_>) {
///     let _ = ctx.add_cleanup(|| {});
/// }
/// ```
pub struct PmContext<'a> {
    device: Arc<DeviceObject>,
    binding: BindingRef,
    _permit: &'a ContextPermit<'a>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'a> PmContext<'a> {
    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are part of the typed ABI but have no \
                      kpm caller in this ticket"
        )
    )]
    pub(crate) fn new(
        device: Arc<DeviceObject>,
        binding: BindingRef,
        permit: &'a ContextPermit<'a>,
    ) -> Self {
        Self {
            device,
            binding,
            _permit: permit,
            _not_send_sync: PhantomData,
        }
    }

    /// Stable device object.
    pub fn device(&self) -> &Arc<DeviceObject> {
        &self.device
    }

    /// Exact binding instance.
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are reserved and tested only in unit \
                      builds"
        )
    )]
    pub(crate) fn take_data(&self) -> Result<Option<DriverData>, DataTakeError> {
        self.device.take_data(self.binding.runtime())
    }

    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are reserved and tested only in unit \
                      builds"
        )
    )]
    pub(crate) fn restore_data(&self, data: DriverData) -> Result<(), DataInstallError> {
        self.device.restore_data(self.binding.runtime(), data)
    }

    #[cfg(unittest)]
    pub fn with_test_context<R>(
        device: Arc<DeviceObject>,
        f: impl FnOnce(&PmContext<'_>) -> R,
    ) -> Option<R> {
        let binding = device.binding_ref_for_lifecycle_write()?;
        let permit = ContextPermit::for_test();
        let ctx = PmContext::new(device, binding, &permit);
        Some(f(&ctx))
    }
}
