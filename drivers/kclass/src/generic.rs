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
use kdevice::{BusId, DeviceCore, DeviceId, DeviceLocation, DeviceObject, DeviceState, DriverId};

/// A typed runtime capability with a live parent device object.
pub struct ClassDevice<T> {
    inner: Arc<ClassDeviceInner<T>>,
}

struct ClassDeviceInner<T> {
    parent: Arc<DeviceObject>,
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

    pub(crate) fn try_new_with_class_metadata(
        parent: Arc<DeviceObject>,
        inner: T,
        name: String,
        device_kind: DeviceKind,
        irq: Option<usize>,
        metadata: ClassDeviceMetadata,
    ) -> DriverResult<Self> {
        parent.driver_name().ok_or(DriverError::BadState)?;
        parent.driver_id().ok_or(DriverError::BadState)?;
        Ok(Self {
            inner: Arc::new(ClassDeviceInner {
                parent,
                runtime: inner,
                name,
                device_kind,
                irq,
                metadata,
            }),
        })
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

    /// Name of the driver that activated this device.
    pub fn driver_name(&self) -> &'static str {
        self.inner
            .parent
            .driver_name()
            .expect("class device must have a bound driver")
    }

    /// The registered driver that activated this device.
    pub fn driver_id(&self) -> DriverId {
        self.inner
            .parent
            .driver_id()
            .expect("class device must have a bound driver")
    }

    /// Return the long-lived core handle associated with this runtime object.
    pub fn core(&self) -> DeviceCore {
        DeviceCore::new(self.id())
    }

    /// Borrow the runtime capability.
    ///
    /// The runtime object is shared immutably; any mutation is the driver's
    /// responsibility via interior locking.
    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.inner.runtime)
    }

    /// Whether the parent device is active and not removed.
    pub fn is_available(&self) -> bool {
        self.inner.parent.state() == DeviceState::Active
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

/// Typed registry for one runtime device class.
pub struct ClassRegistry<T> {
    devices: Vec<ClassDevice<T>>,
    availability_subscribers: Vec<ClassAvailabilityCallback<T>>,
}

impl<T> ClassRegistry<T> {
    /// Create an empty class registry.
    pub const fn new() -> Self {
        Self {
            devices: Vec::new(),
            availability_subscribers: Vec::new(),
        }
    }

    /// Publish or replace one runtime class device.
    pub fn publish(&mut self, device: ClassDevice<T>) {
        let id = device.id();
        if let Some(existing) = self.devices.iter_mut().find(|existing| existing.id() == id) {
            *existing = device;
        } else {
            self.devices.push(device);
        }
    }

    /// Return currently active devices in this class.
    pub fn devices(&self) -> Vec<ClassDevice<T>> {
        self.devices
            .iter()
            .filter(|device| device.is_available())
            .cloned()
            .collect()
    }

    /// Find a currently active device in this class.
    pub fn find(&self, id: DeviceId) -> Option<ClassDevice<T>> {
        self.devices
            .iter()
            .find(|device| device.id() == id && device.is_available())
            .cloned()
    }

    /// Subscribe to future availability notifications for this class.
    pub fn subscribe_available(&mut self, callback: ClassAvailabilityCallback<T>) {
        self.availability_subscribers.push(callback);
    }

    /// Snapshot availability callbacks. Callers should invoke them outside locks.
    pub fn availability_callbacks(&self) -> Vec<ClassAvailabilityCallback<T>> {
        self.availability_subscribers.clone()
    }

    /// Remove a class device by parent device ID.
    pub fn remove(&mut self, id: DeviceId) {
        if let Some(pos) = self.devices.iter().position(|device| device.id() == id) {
            self.devices.swap_remove(pos);
        }
    }
}

impl<T> Default for ClassRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}
