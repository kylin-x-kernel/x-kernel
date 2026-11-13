// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic runtime device class primitives.
//!
//! This module owns the class-device handle and typed registry mechanics.
//! Concrete class adapters such as block, net, char, display, input, vsock,
//! and 9P are wired in the crate root.

use alloc::{string::String, sync::Arc, vec::Vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{
    BindingRef, BusId, DeviceCore, DeviceId, DeviceLocation, DeviceObject, DeviceState, DriverId,
};

/// A typed runtime capability with a live parent device object.
pub struct ClassDevice<T> {
    inner: Arc<ClassDeviceInner<T>>,
}

struct ClassDeviceInner<T> {
    parent: Arc<DeviceObject>,
    /// Binding instance captured when this class device was constructed.
    ///
    /// Identity getters read this immutable snapshot and availability reads
    /// this exact runtime's lifecycle; neither goes back to the parent's
    /// current driver metadata or binding slot.
    binding: BindingRef,
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
    /// Production publication must use [`Self::try_new_with_binding`] with the
    /// handle captured by `binding_ref_for_lifecycle_write()`: a raw stable
    /// object can be held long after its binding was replaced, and reading the
    /// slot here would silently attribute the handle to whichever binding is
    /// current at construction time.
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
    pub(crate) fn try_new_with_binding(
        parent: Arc<DeviceObject>,
        binding: BindingRef,
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
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                parent,
                binding,
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

    /// Whether the captured binding is currently `Active`.
    ///
    /// This observes only the binding instance captured at construction; it
    /// does not query the parent's current state or consume an in-use
    /// admission. Treat it as an instantaneous availability check, not as a
    /// guarantee that a later hardware operation is safe.
    pub fn is_available(&self) -> bool {
        self.inner.binding.is_active()
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

/// Publish barrier for one class-retirement sequence.
///
/// The entry is removed from `devices` before the runtime is unpublished and
/// stays reserved until that unpublish returns. `token` is a private cleanup
/// credential: it is never exposed through a `DeviceId`, a `BindingRef` or any
/// public binding API. It only proves to [`ClassRegistry::finish_retirement`]
/// that the caller is finishing the sequence it claimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClassRetirement {
    id: DeviceId,
    token: u64,
}

/// Result of trying to take responsibility for retiring terminal class entries.
///
/// The type itself has exactly three outcomes; an unrepresentable credential
/// allocation is reported as [`DriverError::BadState`] by the caller-visible
/// method, before any entry is removed.
pub(crate) enum ClaimOutcome<T> {
    /// This caller now owns retirement of `removed` and must publish nothing
    /// for the same `id` until [`ClassRegistry::finish_retirement`] succeeds.
    Claimed {
        id: DeviceId,
        token: u64,
        removed: Vec<ClassDevice<T>>,
    },
    /// Another executor holds the same-ID retirement reservation.
    Retiring,
    /// There is no matching terminal entry; another path already handled it.
    Nothing,
}

impl<T> ClaimOutcome<T> {
    /// Class devices whose runtime this claim is responsible for unpublishing.
    ///
    /// Empty for the `Retiring` and `Nothing` outcomes.
    pub(crate) fn removed(&self) -> &[ClassDevice<T>] {
        match self {
            Self::Claimed { removed, .. } => removed,
            Self::Retiring | Self::Nothing => &[],
        }
    }
}

/// Typed registry for one runtime device class.
pub(crate) struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
    /// Same-ID publish barriers held between "removed from `devices`" and
    /// "unpublish finished". Small in practice, so a linear scan is used to
    /// keep the type allocation-light.
    retiring: Vec<ClassRetirement>,
    next_retirement_token: u64,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub(crate) const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
            retiring: Vec::new(),
            next_retirement_token: 0,
        }
    }

    fn is_retiring(&self, id: DeviceId) -> bool {
        self.retiring.iter().any(|entry| entry.id == id)
    }

    /// Publish one runtime class device.
    ///
    /// Linux device registration rejects duplicate identities; replacement is
    /// an explicit eviction-then-add lifecycle rather than an implicit registry
    /// update. A same-ID retirement reservation rejects publication with
    /// `ResourceBusy` even when this call bypasses the class-level eviction, so
    /// B can never become visible before A's unpublish returns.
    pub(crate) fn publish(&mut self, device: ClassDevice<T>) -> DriverResult<()> {
        let id = device.id();
        if self.is_retiring(id) {
            return Err(DriverError::ResourceBusy);
        }
        if self.devices.iter().any(|existing| existing.id() == id) {
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

    /// Find a currently active device in this class.
    pub(crate) fn find(&self, id: DeviceId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    /// Subscribe to future availability notifications for this class.
    pub(crate) fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub(crate) fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Snapshot the terminal (`Removed`) entries for `id`, without removing or
    /// unpublishing anything.
    ///
    /// Only `Removed` is terminal; a transient `Removing` entry is neither
    /// evicted nor reported as an error to the caller beyond `AlreadyExists`.
    pub(crate) fn terminal_entries_for_retirement(
        &self,
        id: DeviceId,
    ) -> DriverResult<Vec<ClassDevice<T>>> {
        if self.is_retiring(id) {
            return Err(DriverError::ResourceBusy);
        }
        if self
            .devices
            .iter()
            .any(|device| device.id() == id && !device.binding_ref().is_terminal())
        {
            return Err(DriverError::AlreadyExists);
        }
        Ok(self
            .devices
            .iter()
            .filter(|device| device.id() == id && device.binding_ref().is_terminal())
            .cloned()
            .collect())
    }

    /// Take responsibility for retiring entries whose captured runtime is one
    /// of `bindings` and is terminal.
    ///
    /// On `Claimed`, the entries have been removed from the registry and a
    /// same-ID reservation has been installed in the same critical section. The
    /// caller must unpublish the returned runtimes outside the registry lock
    /// and then call [`Self::finish_retirement`].
    ///
    /// # Errors
    ///
    /// Returns `BadState` if a retirement token cannot be allocated. The check
    /// happens before any entry is removed, so a failure leaves the registry
    /// untouched and never wraps or reuses a token.
    pub(crate) fn claim_retirement(
        &mut self,
        id: DeviceId,
        bindings: &[BindingRef],
    ) -> DriverResult<ClaimOutcome<T>> {
        if self.is_retiring(id) {
            return Ok(ClaimOutcome::Retiring);
        }
        let matches = |device: &ClassDevice<T>| {
            device.id() == id
                && device.binding_ref().is_terminal()
                && bindings
                    .iter()
                    .any(|binding| device.binding_ref().same_binding(binding))
        };
        if !self.devices.iter().any(&matches) {
            return Ok(ClaimOutcome::Nothing);
        }
        let token = self
            .next_retirement_token
            .checked_add(1)
            .ok_or(DriverError::BadState)?;
        self.next_retirement_token = token;

        let mut removed = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            if matches(&self.devices[index]) {
                removed.push(self.devices.swap_remove(index));
            } else {
                index += 1;
            }
        }
        self.retiring.push(ClassRetirement { id, token });
        Ok(ClaimOutcome::Claimed { id, token, removed })
    }

    /// Release the retirement reservation described by `claim`.
    ///
    /// Only the reservation with the matching token is removed, so a delayed
    /// finisher cannot clear a later executor's reservation. A claim that was
    /// not `Claimed` is a no-op.
    pub(crate) fn finish_retirement(&mut self, claim: &ClaimOutcome<T>) {
        let ClaimOutcome::Claimed { id, token, .. } = claim else {
            return;
        };
        if let Some(pos) = self
            .retiring
            .iter()
            .position(|entry| entry.id == *id && entry.token == *token)
        {
            self.retiring.remove(pos);
        }
    }

    /// Test-only snapshot of every entry for `id`, regardless of state.
    #[cfg(unittest)]
    pub(crate) fn entries_for_tests(&self, id: DeviceId) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.id() == id)
            .cloned()
            .collect()
    }

    /// Test-only: pretend the previous retirement token was `token`.
    ///
    /// Used to drive the allocator to its `BadState` boundary without
    /// performing `u64::MAX` claims.
    #[cfg(unittest)]
    pub(crate) fn set_next_retirement_token_for_tests(&mut self, token: u64) {
        self.next_retirement_token = token;
    }

    /// Test-only cleanup: drop every entry for `id` whose captured binding is
    /// not listed in `keep`, and clear the ID's retirement reservation.
    ///
    /// Entries in `keep` are those whose node removal was deferred; their
    /// registered completion callback must still be allowed to claim and
    /// unpublish them, so they stay in the registry. Callers must have joined
    /// every task that could hold a claim; this must not be used to paper over
    /// a live publisher or a leaked completion.
    #[cfg(unittest)]
    pub(crate) fn remove_any_for_tests(
        &mut self,
        id: DeviceId,
        keep: &[BindingRef],
    ) -> Vec<ClassDevice<T>> {
        self.retiring.retain(|entry| entry.id != id);
        let matches_keep = |device: &ClassDevice<T>| {
            keep.iter()
                .any(|binding| device.binding_ref().same_binding(binding))
        };
        let mut removed = Vec::new();
        let mut index = 0;
        while index < self.devices.len() {
            if self.devices[index].id() == id && !matches_keep(&self.devices[index]) {
                removed.push(self.devices.swap_remove(index));
            } else {
                index += 1;
            }
        }
        removed
    }
}

impl<T> Default for ClassRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/generic.rs");
