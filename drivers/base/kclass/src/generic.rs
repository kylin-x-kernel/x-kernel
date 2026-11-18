// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic runtime device class primitives.
//!
//! This module owns the class-device handle and typed registry mechanics.
//! Concrete class adapters such as block, net, char, display, input, vsock,
//! and 9P are wired in the crate root.

use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{
    BindingRef, BusId, DeviceCore, DeviceId, DeviceLocation, DeviceObject, DeviceState, DriverId,
    PublicationId,
};

/// A typed runtime capability with a live parent device object.
pub struct ClassDevice<T> {
    inner: Arc<ClassDeviceInner<T>>,
}

/// Non-cloneable credential for explicitly revoking one publication.
pub struct ClassPublication<T> {
    device: ClassDevice<T>,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationState {
    Published    = 0,
    Activating   = 1,
    Notifying    = 2,
    Available    = 3,
    Revoking     = 4,
    RevokeFailed = 5,
    Revoked      = 6,
}

impl PublicationState {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Published,
            1 => Self::Activating,
            2 => Self::Notifying,
            3 => Self::Available,
            4 => Self::Revoking,
            5 => Self::RevokeFailed,
            6 => Self::Revoked,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: only PublicationState \
                          discriminants are stored"
            )]
            _ => panic!("invalid publication state"),
        }
    }
}

struct ClassDeviceInner<T> {
    parent: Arc<DeviceObject>,
    /// Binding instance captured when this class device was constructed.
    ///
    /// Identity getters read this immutable snapshot and availability reads
    /// this exact runtime's lifecycle; neither goes back to the parent's
    /// current driver metadata or binding slot.
    binding: BindingRef,
    publication_id: PublicationId,
    /// Publication state machine. Writers publish the state (and the fact that
    /// node/callback bookkeeping for that state is complete) with Release or
    /// AcqRel; readers in `state()` use Acquire. This pairs every
    /// `Activating`/`Notifying`/`Available` visibility decision with the
    /// preceding activation/node setup, and every `Revoking`/`Revoked`
    /// decision with the exact executor handoff.
    publication_state: AtomicU8,
    /// Set by `request_revoke` under the class-registry lock with Release.
    /// `is_available`, `find_*_publication` and device enumeration observe it
    /// with Acquire so a revoke request closes visibility immediately, before
    /// deferred node/unpublish cleanup finishes.
    revoke_requested: AtomicBool,
    runtime: T,
    name: String,
    device_kind: DeviceKind,
    irq: Option<usize>,
    metadata: ClassDeviceMetadata,
}

pub(crate) struct ClassDeviceMetadata {
    input_identity: Option<InputIdentity>,
}

struct InputIdentity {
    physical_location: String,
    unique_id: String,
}

impl ClassDeviceMetadata {
    pub(crate) const fn empty() -> Self {
        Self {
            input_identity: None,
        }
    }

    pub(crate) fn input(physical_location: String, unique_id: String) -> Self {
        Self {
            input_identity: Some(InputIdentity {
                physical_location,
                unique_id,
            }),
        }
    }
}

impl<T> Clone for ClassDevice<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> ClassDevice<T> {
    /// Create a class device with runtime-provided identity metadata.
    ///
    /// Test-only fixture path: it reads the parent's current binding slot and
    /// is intentionally unavailable to production callers.
    #[cfg(unittest)]
    pub fn try_new_with_info(
        parent: Arc<DeviceObject>,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
    ) -> DriverResult<Self> {
        Self::try_new_with_class_metadata(
            parent,
            inner,
            name,
            device_kind,
            irq,
            ClassDeviceMetadata::empty(),
        )
    }

    /// Fixture/test constructor that reads the parent's current binding slot.
    ///
    /// Production publication uses [`Self::try_new_for_publication`] with the
    /// binding captured in the driver context: a raw stable object can be held
    /// long after its binding was replaced, and reading the slot here would
    /// silently attribute the handle to whichever binding is current at
    /// construction time.
    #[cfg(unittest)]
    pub(crate) fn try_new_with_class_metadata(
        parent: Arc<DeviceObject>,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        // Capture identity and the binding instance together. A Bound device
        // (the normal in-probe publication stage) has complete metadata and is
        // accepted; terminal or detached bindings are not.
        let binding = parent.binding_ref().ok_or(DriverError::BadState)?;
        Self::try_new_with_binding(parent, binding, inner, name, device_kind, irq, metadata)
    }

    /// Construct a class device from an explicitly captured binding instance.
    ///
    /// The caller supplies the `BindingRef` it obtained in a lifecycle write
    /// context (probe/remove/adoption), so the class identity is pinned to
    /// exactly the binding that is publishing it, never to a later replacement
    /// in the parent's slot.
    #[cfg(unittest)]
    pub(crate) fn try_new_with_binding(
        parent: Arc<DeviceObject>,
        binding: BindingRef,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        let publication_id = PublicationId::allocate()?;
        let device = Self::try_new_for_publication(
            parent,
            binding,
            publication_id,
            inner,
            name,
            device_kind,
            irq,
            metadata,
        )?;
        // Test-only convenience constructor: a synthetic handle over an
        // already-Active fixture is immediately usable. Production publish
        // always uses try_new_for_publication and starts Published so the
        // normal activation path registers nodes/notifies subscribers.
        if device.binding_ref().is_active() {
            device.inner.publication_state.store(
                PublicationState::Available as u8,
                core::sync::atomic::Ordering::Release,
            );
        }
        Ok(device)
    }

    /// Construct one publication with an already allocated identity.
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: clippy::too_many_arguments: constructor keeps \
                  publication fields explicit"
    )]
    pub(crate) fn try_new_for_publication(
        parent: Arc<DeviceObject>,
        binding: BindingRef,
        publication_id: PublicationId,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        if matches!(
            binding.state(),
            DeviceState::Removing | DeviceState::Removed
        ) {
            return Err(DriverError::BadState);
        }
        // Even on an already-Active binding, a new publication must start
        // Published so the normal activation path registers its node and
        // notifies subscribers exactly once.
        let initial_state = PublicationState::Published;
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                parent,
                binding,
                publication_id,
                publication_state: AtomicU8::new(initial_state as u8),
                revoke_requested: AtomicBool::new(false),
                runtime: inner,
                name,
                device_kind,
                irq,
                metadata,
            }),
        })
    }

    /// The exact binding instance captured by this class handle.
    ///
    /// Used to revoke the matching device node and to select terminal entries
    /// during retirement. The reference identifies one instance; it grants no
    /// lifecycle write permission.
    pub fn binding_ref(&self) -> &BindingRef {
        &self.inner.binding
    }

    /// Identity of this publication within the parent binding.
    pub fn publication_id(&self) -> PublicationId {
        self.inner.publication_id
    }

    #[cfg(unittest)]
    pub fn publication_state_name(&self) -> &'static str {
        match self.state() {
            PublicationState::Published => "Published",
            PublicationState::Activating => "Activating",
            PublicationState::Notifying => "Notifying",
            PublicationState::Available => "Available",
            PublicationState::Revoking => "Revoking",
            PublicationState::RevokeFailed => "RevokeFailed",
            PublicationState::Revoked => "Revoked",
        }
    }

    fn state(&self) -> PublicationState {
        PublicationState::from_u8(self.inner.publication_state.load(Ordering::Acquire))
    }

    pub(crate) fn begin_activation(&self) -> bool {
        self.inner
            .publication_state
            .compare_exchange(
                PublicationState::Published as u8,
                PublicationState::Activating as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn revoke_requested(&self) -> bool {
        self.inner.revoke_requested.load(Ordering::Acquire)
    }

    pub(crate) fn commit_notification(&self) -> bool {
        if self.revoke_requested() {
            return false;
        }
        self.inner
            .publication_state
            .compare_exchange(
                PublicationState::Activating as u8,
                PublicationState::Notifying as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn finish_notification(&self) -> bool {
        if self.revoke_requested() {
            return false;
        }
        self.inner
            .publication_state
            .compare_exchange(
                PublicationState::Notifying as u8,
                PublicationState::Available as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn take_activation_revoke(&self) -> bool {
        let state = self.state();
        if !matches!(
            state,
            PublicationState::Activating | PublicationState::Notifying
        ) {
            return false;
        }
        self.inner
            .publication_state
            .compare_exchange(
                state as u8,
                PublicationState::Revoking as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub(crate) fn request_revoke(&self) -> bool {
        self.inner.revoke_requested.store(true, Ordering::Release);
        matches!(
            self.state(),
            PublicationState::Activating | PublicationState::Notifying
        )
    }

    pub(crate) fn begin_executor(&self) -> bool {
        self.inner
            .publication_state
            .compare_exchange(
                PublicationState::Published as u8,
                PublicationState::Revoking as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
            || self
                .inner
                .publication_state
                .compare_exchange(
                    PublicationState::Available as u8,
                    PublicationState::Revoking as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            || self
                .inner
                .publication_state
                .compare_exchange(
                    PublicationState::RevokeFailed as u8,
                    PublicationState::Revoking as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
    }

    pub(crate) fn mark_revoke_failed(&self) {
        let _ = self.inner.publication_state.compare_exchange(
            PublicationState::Revoking as u8,
            PublicationState::RevokeFailed as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn finish_revoked(&self) {
        self.inner
            .publication_state
            .store(PublicationState::Revoked as u8, Ordering::Release);
    }

    /// Parent device object in the driver-core object graph.
    pub fn parent(&self) -> Arc<DeviceObject> {
        self.inner.parent.clone()
    }

    /// Unique device-model ID inherited from the parent.
    pub fn id(&self) -> DeviceId {
        self.inner.parent.id()
    }

    /// Stable runtime device name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Runtime capability kind.
    pub fn device_kind(&self) -> DeviceKind {
        self.inner.device_kind
    }

    /// Optional IRQ associated with the runtime capability.
    pub fn irq(&self) -> Option<usize> {
        self.inner.irq
    }

    /// The bus instance this device belongs to.
    pub fn bus_id(&self) -> BusId {
        self.inner.parent.bus_id()
    }

    /// Where the device lives on the bus topology.
    pub fn location(&self) -> DeviceLocation {
        self.inner.parent.location()
    }

    /// Name of the driver that created this class device.
    ///
    /// This is an immutable snapshot captured at class construction. It stays
    /// readable after the parent device is unbound or removed and never reads
    /// the parent's current, detachable metadata.
    pub fn driver_name(&self) -> &'static str {
        self.inner.binding.driver_name()
    }

    /// ID of the driver that created this class device.
    ///
    /// Like [`Self::driver_name`], this is the historical identity of the
    /// binding captured at construction, not a live parent query.
    pub fn driver_id(&self) -> DriverId {
        self.inner.binding.driver_id()
    }

    /// Return the long-lived core handle associated with this runtime object.
    pub fn core(&self) -> DeviceCore {
        DeviceCore::new(self.id())
    }

    /// Borrow the runtime capability.
    ///
    /// The runtime object is shared immutably; any mutation is the driver's
    /// responsibility via interior locking.
    pub fn with<'a, R>(&'a self, f: impl FnOnce(&'a T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Selects an object stored inside the runtime capability.
    ///
    /// The runtime capability owns the selected object and never moves or
    /// replaces it while published, so the borrow is valid for the lifetime of
    /// this handle. This form exists because the borrow checker rejects
    /// returning a sub-borrow through a plain `with` closure: the closure's
    /// result type cannot mention the borrow's lifetime.
    pub fn select<'a, U: ?Sized + 'a, R>(
        &'a self,
        select: impl FnOnce(&'a T) -> Option<&'a U>,
        map: impl FnOnce(Option<&'a U>) -> R,
    ) -> R {
        map(select(&self.inner.runtime))
    }

    /// Whether this publication currently accepts new use.
    ///
    /// True only for `Activating`/`Notifying`/`Available`, with no revoke
    /// request, while the captured binding is `Active`. A revoke request
    /// closes this immediately, even if cleanup or an already committed
    /// callback is still pending. This observes only the captured binding; it
    /// does not query the parent's current state or consume an in-use
    /// admission, and it is not a guarantee that a later hardware operation is
    /// safe.
    pub fn is_available(&self) -> bool {
        !self.revoke_requested()
            && matches!(
                self.state(),
                PublicationState::Activating
                    | PublicationState::Notifying
                    | PublicationState::Available
            )
            && self.inner.binding.is_active()
    }

    pub(crate) fn input_physical_location(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.physical_location.as_str())
    }

    pub(crate) fn input_unique_id(&self) -> &str {
        self.inner
            .metadata
            .input_identity
            .as_ref()
            .map_or("", |identity| identity.unique_id.as_str())
    }
}

impl<T> ClassPublication<T> {
    pub(crate) fn new(device: ClassDevice<T>) -> Self {
        Self { device }
    }

    /// Consumer handle for this publication.
    pub fn device(&self) -> ClassDevice<T> {
        self.device.clone()
    }

    /// Parent device identity.
    pub fn id(&self) -> DeviceId {
        self.device.id()
    }

    /// Publication identity.
    pub fn publication_id(&self) -> PublicationId {
        self.device.publication_id()
    }

    /// Exact binding captured by the publication.
    pub fn binding_ref(&self) -> &BindingRef {
        self.device.binding_ref()
    }

    /// Whether this publication still accepts new use.
    pub fn is_available(&self) -> bool {
        self.device.is_available()
    }

    /// Revoke this publication.
    ///
    /// The credential is intentionally not `Clone`; dropping it does not
    /// revoke. The caller must be in task context suitable for the class
    /// backend's unpublish operation.
    pub fn revoke(&self) -> DriverResult<RevocationOutcome> {
        crate::revoke_publication(self.device.device_kind(), self.device.publication_id())
    }
}

impl<T> core::ops::Deref for ClassPublication<T> {
    type Target = ClassDevice<T>;

    fn deref(&self) -> &Self::Target {
        &self.device
    }
}

impl<T> Device for ClassDevice<T>
where
    T: Send + Sync,
{
    fn name(&self) -> &str {
        self.name()
    }

    fn device_kind(&self) -> DeviceKind {
        self.device_kind()
    }

    fn irq(&self) -> Option<usize> {
        self.irq()
    }
}

impl<T> core::fmt::Debug for ClassDevice<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClassDevice")
            .field("id", &self.id())
            .field("bus_id", &self.bus_id())
            .field("location", &self.location())
            .field("driver_name", &self.driver_name())
            .field("driver_id", &self.driver_id())
            .finish()
    }
}

/// Notification callback invoked after a typed runtime capability is activated.
pub type ClassAvailabilityCallback<T> = Arc<dyn Fn(ClassDevice<T>) + Send + Sync>;
/// Callback invoked after one publication is fully revoked.
pub type ClassRemovalCallback = Arc<dyn Fn(DeviceId, PublicationId) + Send + Sync>;

/// Result of an explicit publication revocation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevocationOutcome {
    /// Node is silent, backend unpublish and removed callbacks completed.
    Completed,
    /// A live activation/revocation executor owns completion.
    Deferred,
    /// The publication is already fully revoked.
    AlreadyRevoked,
}

pub(crate) enum RevocationStart<T> {
    Execute(ClassDevice<T>),
    Deferred,
    AlreadyRevoked,
    Failed,
}

/// Typed registry for one runtime device class.
pub(crate) struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    removed_subscribers: Vec<ClassRemovalCallback>,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub(crate) const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            removed_subscribers: Vec::new(),
        }
    }

    /// Publish one exact runtime class publication.
    ///
    /// The key is `PublicationId`, not `DeviceId`: the same stable device and
    /// binding may own several capabilities of this class. A duplicate
    /// publication identity is rejected, but sibling publications are
    /// independent.
    pub(crate) fn publish(&mut self, device: ClassDevice<T>) -> DriverResult<()> {
        if self
            .devices
            .iter()
            .any(|existing| existing.publication_id() == device.publication_id())
        {
            return Err(DriverError::AlreadyExists);
        }
        self.devices.push(device);
        Ok(())
    }

    /// Return currently active devices in this class.
    pub(crate) fn devices(&self) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Subscribe to future availability notifications for this class.
    pub(crate) fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub(crate) fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Find one currently available publication by exact identity.
    pub(crate) fn find_publication(&self, publication: PublicationId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.publication_id() == publication && device.is_available())
            .cloned()
    }

    /// Internal lookup including non-available entries.
    pub(crate) fn publication_entry(&self, publication: PublicationId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.publication_id() == publication)
            .cloned()
    }

    /// Internal full-state snapshot for retirement/activation scans.
    pub(crate) fn all_publications_for_device(&self, id: DeviceId) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.id() == id)
            .cloned()
            .collect()
    }

    /// Public device-level enumeration; unavailable/revoking entries are hidden.
    pub(crate) fn available_publications_for_device(&self, id: DeviceId) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.id() == id && device.is_available())
            .cloned()
            .collect()
    }

    pub(crate) fn request_revoke(&mut self, publication: PublicationId) -> RevocationStart<T> {
        let Some(device) = self.publication_entry(publication) else {
            return RevocationStart::AlreadyRevoked;
        };
        let state = device.state();
        if matches!(
            state,
            PublicationState::Activating | PublicationState::Notifying
        ) {
            device.request_revoke();
            return RevocationStart::Deferred;
        }
        if state == PublicationState::Revoking {
            return RevocationStart::Deferred;
        }
        if state == PublicationState::Revoked {
            return RevocationStart::AlreadyRevoked;
        }
        if device.begin_executor() {
            RevocationStart::Execute(device)
        } else {
            RevocationStart::Failed
        }
    }

    pub(crate) fn removed_callbacks(&self) -> Vec<ClassRemovalCallback> {
        self.removed_subscribers.clone()
    }

    pub(crate) fn mark_revoke_failed(&mut self, publication: PublicationId) {
        if let Some(device) = self.publication_entry(publication) {
            device.mark_revoke_failed();
        }
    }

    pub(crate) fn remove_publication(
        &mut self,
        publication: PublicationId,
    ) -> Option<ClassDevice<T>> {
        let index = self
            .devices
            .iter()
            .position(|device| device.publication_id() == publication)?;
        Some(self.devices.swap_remove(index))
    }

    pub(crate) fn subscribe_removed(&mut self, callback: ClassRemovalCallback) {
        self.removed_subscribers.push(callback);
    }

    pub(crate) fn available_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_callbacks()
    }
}

impl<T> Default for ClassRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/generic.rs");
