// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime state and cleanup responsibility of one driver binding.
//!
//! A [`BindingRuntime`] groups everything that belongs to one driver binding:
//! the lifecycle state, in-use admission, the current driver metadata and the
//! cleanup batch registered by that binding. A stable `DeviceObject` replaces
//! the runtime in its slot when a new candidate is probed, but it never resets
//! or reuses a retired runtime. The object holds each instance through `Arc`
//! so consumers can retain the exact instance they were created from.
//!
//! [`BindingRef`] is the read-only handle exposed to consumers such as the
//! class layer. It keeps the runtime alive and snapshots the driver identity.
//! New exact-instance operations (`try_acquire`/`add_cleanup`) check that the
//! captured runtime is still the object's current slot before acting, so an
//! old binding handle can never admit or register cleanup on a replacement.
//! Hardware admission remains a [`DeviceUse`](crate::DeviceUse) responsibility.
//!
//! The descriptor binding mutex in the registry is deliberately **not** part
//! of this type. It serializes probe/adoption/remove/unregister at the
//! registration layer and stays owned by the registry.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use driver_base::{DeviceKind, DriverResult};
use kspin::SpinNoPreempt;

use crate::{DeviceObject, DeviceState, DeviceUse, DriverId};

/// Current driver metadata of one binding.
struct BindingMeta {
    driver_id: Option<DriverId>,
    driver_name: Option<&'static str>,
    device_kind: Option<DeviceKind>,
}

/// One sample of a binding's driver metadata plus a separate lifecycle read,
/// produced by [`BindingRuntime::snapshot`].
///
/// `driver_id`/`driver_name`/`device_kind` are read under one lock and are
/// mutually consistent. `state` is read before that lock and may come from a
/// different instant; it is not jointly consistent with the metadata group or
/// with a separately taken topology snapshot. See
/// `DeviceObject::record_snapshot`.
pub(super) struct BindingSnapshot {
    pub(super) state: DeviceState,
    pub(super) driver_id: Option<DriverId>,
    pub(super) driver_name: Option<&'static str>,
    pub(super) device_kind: Option<DeviceKind>,
}

/// Read-only reference to one binding instance.
///
/// A `BindingRef` keeps the [`BindingRuntime`] alive and snapshots the driver
/// identity captured at creation time. Its operations are exact-instance
/// operations: they first verify that this runtime is still the object's
/// current binding, so a reference captured from binding A can never admit
/// hardware use or register cleanup on a later binding B.
///
/// Holding a `BindingRef` alone never blocks removal. Use
/// [`BindingRef::try_acquire`] when hardware-operational admission is required,
/// and [`BindingRef::add_cleanup`] to attach devres to the captured instance.
///
/// Instances can only be obtained from
/// `DeviceObject::binding_ref()`/`binding_ref_for_lifecycle_write()`; all
/// fields are private and there is no public constructor.
#[derive(Clone)]
pub struct BindingRef {
    runtime: Arc<BindingRuntime>,
    driver_id: DriverId,
    driver_name: &'static str,
}

impl BindingRef {
    /// Lifecycle state of the captured binding instance.
    ///
    /// A lock-free observation of this exact runtime. It does not read the
    /// parent object's current slot, so it keeps reporting A after a
    /// replacement binding B is installed.
    pub fn state(&self) -> DeviceState {
        self.runtime.state()
    }

    /// Whether the captured binding instance has reached its terminal state.
    ///
    /// Only `Removed` is terminal. `Removing` may return to the previous state
    /// when removal backs off because a [`DeviceUse`] guard is outstanding.
    pub fn is_terminal(&self) -> bool {
        matches!(self.state(), DeviceState::Removed)
    }

    /// Whether the captured binding instance is currently `Active`.
    ///
    /// This is an instantaneous, lock-free observation, not an operation
    /// admission. It may become `false` when removal starts and can become
    /// `true` again if a removal attempt backs off because a `DeviceUse`
    /// guard is still outstanding.
    pub fn is_active(&self) -> bool {
        matches!(self.state(), DeviceState::Active)
    }

    /// Driver ID snapshotted when this reference was captured.
    pub fn driver_id(&self) -> DriverId {
        self.driver_id
    }

    /// Driver name snapshotted when this reference was captured.
    pub fn driver_name(&self) -> &'static str {
        self.driver_name
    }

    /// Whether this handle captures the same runtime instance as `other`.
    ///
    /// This is an identity comparison only. It grants no admission and no
    /// cleanup permission; those operations still check that the captured
    /// runtime is the object's current binding.
    pub fn same_binding(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.runtime, &other.runtime)
    }

    /// Admit one in-use reference to the captured binding instance.
    ///
    /// Fails if the parent object's current binding has been replaced (this
    /// is the A→B isolation rule) or if this exact instance is not `Active`.
    pub fn try_acquire(&self, device: &Arc<DeviceObject>) -> Option<DeviceUse> {
        device.try_acquire_exact(&self.runtime)
    }

    /// Register a cleanup callback on the captured binding instance.
    ///
    /// The object must still have this exact runtime in its binding slot. For
    /// a registered object the caller must hold the descriptor binding lock
    /// (probe/remove/adoption write context); detached fixtures do not.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError::BadState`](crate::DriverError::BadState) if the
    /// slot no longer points at this instance, the caller is not in a
    /// lifecycle write context, or the instance has already started removal.
    pub fn add_cleanup<F>(&self, device: &DeviceObject, cleanup: F) -> DriverResult<()>
    where
        F: FnOnce() + Send + 'static,
    {
        device.add_cleanup_exact(&self.runtime, Box::new(cleanup))
    }
}

impl core::fmt::Debug for BindingRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BindingRef")
            .field("driver_id", &self.driver_id)
            .field("driver_name", &self.driver_name)
            .field("is_active", &self.is_active())
            .finish()
    }
}

/// Runtime state and cleanup responsibility of one driver binding.
///
/// Only the device core mutates this type; it is not exported from the crate
/// and has no replace/reset/into-inner API. The type is crate-visible only so
/// `DeviceObject::clear_binding` can return the retired instance to the
/// lifecycle code that calls it.
pub(crate) struct BindingRuntime {
    lifecycle: AtomicU8,
    /// Outstanding in-use references handed out by `BindingRef::try_acquire`.
    ///
    /// Removal is gated on this dropping back to zero so a consumer holding a
    /// live reference (an opened handle, an in-flight operation) cannot have
    /// the device torn down underneath it.
    usage: AtomicUsize,
    /// Driver metadata; cleared when the binding is detached.
    meta: SpinNoPreempt<BindingMeta>,
    /// Driver-registered cleanup callbacks ("devres"), run in LIFO order when
    /// a probe fails or the device is removed.
    devres: SpinNoPreempt<Vec<Box<dyn FnOnce() + Send>>>,
}

impl BindingRuntime {
    /// Create the runtime for a newly discovered, unbound device object.
    pub(super) const fn new() -> Self {
        Self {
            lifecycle: AtomicU8::new(DeviceState::Discovered.as_u8()),
            usage: AtomicUsize::new(0),
            meta: SpinNoPreempt::new(BindingMeta {
                driver_id: None,
                driver_name: None,
                device_kind: None,
            }),
            devres: SpinNoPreempt::new(Vec::new()),
        }
    }

    /// Current lifecycle state — lock-free atomic read.
    ///
    /// The atomic is only ever written via [`DeviceState::as_u8`], so an
    /// undecodable value is impossible barring memory corruption; such a value
    /// is treated as [`DeviceState::Removed`] so callers stop operating on the
    /// object instead of panicking on a hot read path.
    pub(super) fn state(&self) -> DeviceState {
        DeviceState::from_u8(self.lifecycle.load(Ordering::Acquire)).unwrap_or(DeviceState::Removed)
    }

    /// Bound driver ID, if any.
    pub(super) fn driver_id(&self) -> Option<DriverId> {
        self.meta.lock().driver_id
    }

    /// Bound driver name, if any.
    pub(super) fn driver_name(&self) -> Option<&'static str> {
        self.meta.lock().driver_name
    }

    /// Device subsystem kind, if known.
    pub(super) fn device_kind(&self) -> Option<DeviceKind> {
        self.meta.lock().device_kind
    }

    /// Capture a read-only reference to this binding instance.
    ///
    /// The driver id and name are sampled together under one `meta` critical
    /// section, and the state check happens in that same section. A successful
    /// capture only means the binding was not observed as `Removing`/`Removed`
    /// and still had identity metadata at that instant; it is not a publish
    /// permission and does not prevent a later removal from completing.
    pub(super) fn capture_ref(self: &Arc<Self>) -> Option<BindingRef> {
        let meta = self.meta.lock();
        if matches!(self.state(), DeviceState::Removing | DeviceState::Removed) {
            return None;
        }
        let driver_id = meta.driver_id?;
        let driver_name = meta.driver_name?;
        drop(meta);
        Some(BindingRef {
            runtime: Arc::clone(self),
            driver_id,
            driver_name,
        })
    }

    /// Store a new lifecycle state with release ordering.
    fn store_state(&self, new_state: DeviceState) {
        self.lifecycle.store(new_state.as_u8(), Ordering::Release);
    }

    /// Mark the binding as matched.
    pub(super) fn mark_matched(&self) {
        self.store_state(DeviceState::Matched);
    }

    /// Record the driver metadata and mark the binding as bound.
    pub(super) fn bind(&self, driver_id: DriverId, driver_name: &'static str, kind: DeviceKind) {
        {
            let mut meta = self.meta.lock();
            meta.driver_id = Some(driver_id);
            meta.driver_name = Some(driver_name);
            meta.device_kind = Some(kind);
        }
        self.store_state(DeviceState::Bound);
    }

    /// Mark the binding as active, refreshing the reported device kind.
    pub(super) fn activate(&self, kind: DeviceKind) {
        self.meta.lock().device_kind = Some(kind);
        self.store_state(DeviceState::Active);
    }

    /// Detach any bound driver metadata.
    pub(super) fn detach(&self) {
        let mut meta = self.meta.lock();
        meta.driver_id = None;
        meta.driver_name = None;
        meta.device_kind = None;
    }

    /// Atomically transition into `Removing`. Returns `false` if removal was
    /// already in progress (the lifecycle state is already `Removing` or
    /// `Removed`) or if the binding still has outstanding in-use references.
    ///
    /// Entering `Removing` only closes admission for new references. On a
    /// non-zero usage count the prior state is restored and no teardown is
    /// committed; the caller may retry. Only a `true` return lets the
    /// lifecycle layer start irreversible teardown.
    pub(super) fn begin_removing(&self) -> bool {
        // CAS loop allows concurrent readers and other transitions to make
        // progress without holding the per-object spinlock.
        let mut current = self.lifecycle.load(Ordering::Acquire);
        let prior;
        loop {
            // An undecodable discriminant can only mean corruption; treat it as
            // already-removed and decline the transition rather than panicking.
            let Some(cur_state) = DeviceState::from_u8(current) else {
                return false;
            };
            if matches!(cur_state, DeviceState::Removing | DeviceState::Removed) {
                return false;
            }
            match self.lifecycle.compare_exchange_weak(
                current,
                DeviceState::Removing.as_u8(),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    prior = cur_state;
                    break;
                }
                Err(observed) => current = observed,
            }
        }

        #[cfg(unittest)]
        crate::lifecycle::test_hooks::pause(
            crate::lifecycle::test_hooks::BindingPause::BindingRemovingBeforeUseCheck,
        );

        // The state is now `Removing`, which makes `try_acquire` reject new
        // references. If any are still outstanding we must NOT proceed: no
        // teardown has happened yet, so it is safe to revert to the prior
        // state and report the device as busy.
        if self.usage.load(Ordering::Acquire) != 0 {
            self.store_state(prior);
            return false;
        }
        true
    }

    /// Mark the binding as removed.
    pub(super) fn mark_removed(&self) {
        self.store_state(DeviceState::Removed);
    }

    /// Try to admit one in-use reference.
    ///
    /// Returns `true` only while the binding is `Active`. Ordering the
    /// increment before the state check means a concurrent `begin_removing`
    /// either observes our count (and backs off) or has already moved the
    /// state out of `Active` (and we back off) — never both proceeding.
    pub(super) fn try_acquire(&self) -> bool {
        self.usage.fetch_add(1, Ordering::AcqRel);
        if matches!(self.state(), DeviceState::Active) {
            true
        } else {
            self.usage.fetch_sub(1, Ordering::AcqRel);
            false
        }
    }

    /// Release one in-use reference admitted by [`Self::try_acquire`].
    pub(super) fn release_use(&self) {
        self.usage.fetch_sub(1, Ordering::AcqRel);
    }

    /// Append a cleanup callback to the current binding's cleanup batch.
    pub(super) fn add_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>) {
        self.devres.lock().push(cleanup);
    }

    /// Run and drain the current cleanup batch in LIFO order.
    ///
    /// The callbacks are taken out of the lock before running so a callback is
    /// free to touch the device without re-entering the devres lock. Each item
    /// in the taken batch runs exactly once. A cleanup registered by a running
    /// callback joins the next batch and is not run by this call; only a call
    /// with no new registrations is a no-op.
    pub(super) fn run_cleanups(&self) {
        let mut cleanups = core::mem::take(&mut *self.devres.lock());
        while let Some(cleanup) = cleanups.pop() {
            cleanup();
        }
    }

    /// Sample the lifecycle state and the driver metadata group.
    ///
    /// The lifecycle is read before the metadata lock, matching the previous
    /// snapshot order. The returned metadata fields are mutually consistent;
    /// the lifecycle value is a separate atomic sample and is not guaranteed
    /// to belong to the same instant.
    pub(super) fn snapshot(&self) -> BindingSnapshot {
        let state = self.state();
        let meta = self.meta.lock();
        BindingSnapshot {
            state,
            driver_id: meta.driver_id,
            driver_name: meta.driver_name,
            device_kind: meta.device_kind,
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/binding.rs");
unittest::path_mod_test!(binding_ref_tests, "../../ut-tests/binding_ref.rs");
