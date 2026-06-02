// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus type object, the driver matching domain.

use alloc::{sync::Arc, vec::Vec};

use driver_base::DriverResult;
use kspin::SpinNoPreempt;

use crate::{
    BusId, DeviceDesc, DeviceDescId, DeviceDriver, DeviceIdentity, DeviceLocation, DeviceObject,
    DriverObject, MatchResult,
};

/// Driver-core bus type identifier.
///
/// A stable string handle that identifies one driver matching domain. Built-in
/// bus types use the [`BusTypeId::PCI`] and [`BusTypeId::PLATFORM`] constants;
/// external crates can mint their own handles via [`BusTypeId::new`] with a
/// unique `'static` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BusTypeId(&'static str);

impl BusTypeId {
    /// PCI / PCIe bus type.
    pub const PCI: BusTypeId = BusTypeId("pci");
    /// Linux-like platform bus type for firmware and platform-static devices.
    pub const PLATFORM: BusTypeId = BusTypeId("platform");

    /// Mint a new bus type id from a unique `'static` name.
    pub const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// Stable name of this bus type.
    pub const fn name(self) -> &'static str {
        self.0
    }
}

/// Matching-domain strategy for a Linux-like bus type.
///
/// Built-in bus types are served by [`PciBusTypeMatcher`] and
/// [`PlatformBusTypeMatcher`]. External crates can implement this trait to
/// provide custom matching logic for new bus types.
pub trait BusType: Send + Sync {
    /// Bus type identifier.
    fn id(&self) -> BusTypeId;

    /// Decide whether a driver can bind a device in this domain.
    fn match_desc(&self, driver: &dyn DeviceDriver, desc: &DeviceDesc) -> MatchResult;

    /// Bus-type cleanup hook after the bound driver has removed the device.
    fn remove(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }
}

/// PCI / PCIe bus type matcher.
///
/// Restricts probe candidates to descriptors located on PCI buses and then
/// delegates the match decision to `driver.matcher()`.
pub struct PciBusTypeMatcher;

#[allow(clippy::new_without_default)]
impl PciBusTypeMatcher {
    /// Create a new PCI bus type matcher.
    pub fn new() -> Self {
        Self
    }

    fn accepts_desc(&self, desc: &DeviceDesc) -> bool {
        // `DeviceLocation::Bridge` is intentionally excluded: bridges are
        // adopted directly by their owning backend and never enter the probe
        // queue, so no endpoint driver should match against them.
        matches!(desc.location(), DeviceLocation::Pci { .. })
            && matches!(desc.identity(), DeviceIdentity::Pci(_))
    }
}

impl BusType for PciBusTypeMatcher {
    fn id(&self) -> BusTypeId {
        BusTypeId::PCI
    }

    fn match_desc(&self, driver: &dyn DeviceDriver, desc: &DeviceDesc) -> MatchResult {
        if !driver.bus_types().contains(&BusTypeId::PCI) || !self.accepts_desc(desc) {
            return MatchResult::NoMatch;
        }
        driver.matcher().matches(desc)
    }
}

/// Platform bus type matcher.
///
/// Restricts probe candidates to descriptors located on platform buses
/// (firmware-described, platform-static, MMIO) and then delegates the match
/// decision to `driver.matcher()`.
pub struct PlatformBusTypeMatcher;

#[allow(clippy::new_without_default)]
impl PlatformBusTypeMatcher {
    /// Create a new platform bus type matcher.
    pub fn new() -> Self {
        Self
    }

    fn accepts_desc(&self, desc: &DeviceDesc) -> bool {
        // `DeviceLocation::Bridge` is intentionally excluded here as well; see
        // the note in `PciBusTypeMatcher::accepts_desc`.
        matches!(
            desc.location(),
            DeviceLocation::FirmwareNode { .. }
                | DeviceLocation::PlatformStatic { .. }
                | DeviceLocation::Mmio { .. }
        )
    }
}

impl BusType for PlatformBusTypeMatcher {
    fn id(&self) -> BusTypeId {
        BusTypeId::PLATFORM
    }

    fn match_desc(&self, driver: &dyn DeviceDriver, desc: &DeviceDesc) -> MatchResult {
        if !driver.bus_types().contains(&BusTypeId::PLATFORM) || !self.accepts_desc(desc) {
            return MatchResult::NoMatch;
        }
        driver.matcher().matches(desc)
    }
}

/// Linux-like bus type object.
///
/// Manages bus instances, pending descriptors and registered drivers for
/// one matching domain. The matching strategy is delegated to a `BusType`
/// implementation, allowing hot-loadable modules to plug in custom logic.
pub struct BusTypeObject {
    matcher: Arc<dyn BusType>,
    buses: SpinNoPreempt<Vec<BusId>>,
    pending_descriptors: SpinNoPreempt<Vec<DeviceDescId>>,
    drivers: SpinNoPreempt<Vec<Arc<DriverObject>>>,
}

impl BusTypeObject {
    /// Create a bus type object with a custom matching strategy.
    pub fn new(matcher: Arc<dyn BusType>) -> Self {
        Self {
            matcher,
            buses: SpinNoPreempt::new(Vec::new()),
            pending_descriptors: SpinNoPreempt::new(Vec::new()),
            drivers: SpinNoPreempt::new(Vec::new()),
        }
    }

    /// Create a bus type object with the built-in matching strategy.
    ///
    /// Panics if `id` is not one of the built-in bus types
    /// ([`BusTypeId::PCI`], [`BusTypeId::PLATFORM`]).
    pub fn new_builtin(id: BusTypeId) -> Self {
        if id == BusTypeId::PCI {
            Self::new(Arc::new(PciBusTypeMatcher::new()))
        } else if id == BusTypeId::PLATFORM {
            Self::new(Arc::new(PlatformBusTypeMatcher::new()))
        } else {
            panic!("BusTypeObject::new_builtin: unknown bus type {:?}", id);
        }
    }

    /// Bus type ID.
    pub fn id(&self) -> BusTypeId {
        self.matcher.id()
    }

    /// Decide whether a driver can bind a device in this domain.
    pub fn match_desc(&self, driver: &dyn DeviceDriver, desc: &DeviceDesc) -> MatchResult {
        self.matcher.match_desc(driver, desc)
    }

    /// Bus-type cleanup hook after the bound driver has removed the device.
    pub fn remove(&self, device: Arc<DeviceObject>) -> DriverResult<()> {
        self.matcher.remove(device)
    }

    /// Attach a bus instance to this type.
    pub fn attach_bus(&self, id: BusId) {
        let mut buses = self.buses.lock();
        if !buses.contains(&id) {
            buses.push(id);
        }
    }

    /// Whether this matching domain includes a bus instance.
    pub fn has_bus(&self, id: BusId) -> bool {
        self.buses.lock().contains(&id)
    }

    /// Queue a discovery descriptor that still needs a matching driver.
    pub fn enqueue_pending_descriptor(&self, id: DeviceDescId) {
        let mut pending = self.pending_descriptors.lock();
        if !pending.contains(&id) {
            pending.push(id);
        }
    }

    /// Remove a descriptor from the pending match queue.
    pub fn remove_pending_descriptor(&self, id: DeviceDescId) {
        let mut pending = self.pending_descriptors.lock();
        if let Some(pos) = pending.iter().position(|current| *current == id) {
            pending.swap_remove(pos);
        }
    }

    /// Attach a driver object to this matching domain.
    pub fn attach_driver(&self, driver: Arc<DriverObject>) {
        let mut drivers = self.drivers.lock();
        if !drivers.iter().any(|current| current.id() == driver.id()) {
            drivers.push(driver);
        }
    }

    /// Snapshot registered drivers.
    pub fn drivers_snapshot(&self) -> Vec<Arc<DriverObject>> {
        self.drivers.lock().clone()
    }

    /// Snapshot descriptors waiting for a matching driver.
    pub fn pending_descriptors_snapshot(&self) -> Vec<DeviceDescId> {
        self.pending_descriptors.lock().clone()
    }
}
