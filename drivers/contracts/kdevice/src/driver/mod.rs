// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver identity and driver-core objects shared by the driver framework.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use driver_base::{DeviceKind, DriverResult};
use kspin::SpinNoPreempt;

use crate::{
    BusTypeId, DeviceDesc, DeviceId, DeviceIdentity, DeviceObject, DiscoveryOrigin, TransportInfo,
    device::desc::PlatformIdentity,
};

/// Probe accounting for a driver or bus instance.
///
/// Snapshotted via [`DriverObject::probe_stats`] and
/// [`crate::BusInstance::probe_stats`] to help diagnose drivers or buses
/// with repeated probe failures (see review item D2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProbeStats {
    /// Total probe attempts dispatched.
    pub attempts: u64,
    /// Probe attempts that returned an error.
    pub failures: u64,
}

/// Atomic counters backing a [`ProbeStats`] snapshot.
#[derive(Debug)]
pub(crate) struct ProbeCounters {
    attempts: AtomicU64,
    failures: AtomicU64,
}

impl ProbeCounters {
    pub(crate) const fn new() -> Self {
        Self {
            attempts: AtomicU64::new(0),
            failures: AtomicU64::new(0),
        }
    }

    /// Record one dispatched probe attempt.
    pub(crate) fn record_attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one failed probe attempt.
    pub(crate) fn record_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the current counts.
    pub(crate) fn snapshot(&self) -> ProbeStats {
        ProbeStats {
            attempts: self.attempts.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
        }
    }
}

impl Default for ProbeCounters {
    fn default() -> Self {
        Self::new()
    }
}

/// Globally unique driver identifier assigned at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DriverId(u64);

impl DriverId {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Persistent driver metadata.
#[derive(Debug, Clone)]
pub struct DriverInfo {
    /// Unique driver ID.
    pub id: DriverId,
    /// Human-readable driver name.
    pub name: &'static str,
    /// The device subsystem this driver serves.
    pub device_kind: DeviceKind,
}

/// Result of a driver's match attempt against a discovery descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchResult {
    /// The driver does not support this device.
    NoMatch,
    /// The driver can handle this device. Higher priority wins.
    Match { priority: u8 },
}

/// Standard match priorities used by built-in driver descriptors.
pub mod priority {
    /// Generic fallback match.
    pub const FALLBACK: u8 = 1;
    /// Generic bus-class match.
    pub const GENERIC: u8 = 5;
    /// Stable exact-ID match.
    pub const EXACT: u8 = 10;
}

/// PCI vendor/device ID pair for matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciDeviceId {
    pub vendor_id: u16,
    pub device_id: u16,
}

/// Firmware identity table for one platform driver family.
///
/// The `alias` is the stable kernel-side identity used by platform-static
/// fallback descriptors. Raw firmware identifiers stay split by source so DT,
/// ACPI, and static fallbacks do not accidentally match each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareMatchSpec {
    /// Stable kernel-side identity name used for platform-static fallback devices.
    pub alias: &'static str,
    /// Device Tree `compatible` strings accepted by this driver.
    pub dt_compatibles: &'static [&'static str],
    /// ACPI `_HID` / `_CID` strings accepted by this driver.
    pub acpi_ids: &'static [&'static str],
}

impl FirmwareMatchSpec {
    /// Returns the matched DT compatible if this driver supports it.
    pub fn match_dt(&self, compatible: &str) -> Option<&'static str> {
        self.dt_compatibles
            .iter()
            .copied()
            .find(|candidate| *candidate == compatible)
    }

    /// Returns the matched ACPI `_HID` / `_CID` if this driver supports it.
    pub fn match_acpi(&self, id: &str) -> Option<&'static str> {
        self.acpi_ids
            .iter()
            .copied()
            .find(|candidate| *candidate == id)
    }
}

/// Open-extension device matcher trait.
///
/// Replaces the closed `MatchTable` enum: each driver supplies one matcher
/// (built-in or custom) and the bus type just calls
/// [`DeviceMatcher::matches`]. Built-in matchers ship in this crate; external
/// crates may implement their own.
pub trait DeviceMatcher: Send + Sync + 'static {
    /// Evaluate the matcher against a discovery descriptor.
    fn matches(&self, desc: &DeviceDesc) -> MatchResult;

    /// Firmware match spec carried by this matcher, if any.
    ///
    /// Used by the platform-firmware backend to pre-filter DT/ACPI ids
    /// before any device is registered.
    fn firmware_spec(&self) -> Option<&FirmwareMatchSpec> {
        None
    }
}

/// Match PCI devices by vendor/device ID pair.
pub struct PciIdsMatcher(pub &'static [PciDeviceId]);

impl DeviceMatcher for PciIdsMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        let DeviceIdentity::Pci(pci) = desc.identity() else {
            return MatchResult::NoMatch;
        };
        if self
            .0
            .iter()
            .any(|id| id.vendor_id == pci.vendor_id && id.device_id == pci.device_id)
        {
            MatchResult::Match {
                priority: priority::EXACT,
            }
        } else {
            MatchResult::NoMatch
        }
    }
}

/// Match VirtIO transport (PCI or MMIO) by VirtIO device type code.
pub struct VirtioTypeMatcher {
    pub device_type: u32,
}

impl DeviceMatcher for VirtioTypeMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        match desc.transport() {
            Some(TransportInfo::Virtio { device_type }) if device_type == self.device_type => {
                MatchResult::Match {
                    priority: priority::EXACT,
                }
            }
            _ => MatchResult::NoMatch,
        }
    }
}

/// Match a platform-static device by its compatible alias string.
pub struct CompatibleAliasMatcher(pub &'static str);

impl CompatibleAliasMatcher {
    fn match_alias(&self, platform: &PlatformIdentity) -> MatchResult {
        if platform.alias == Some(self.0) {
            MatchResult::Match {
                priority: priority::EXACT,
            }
        } else {
            MatchResult::NoMatch
        }
    }
}

impl DeviceMatcher for CompatibleAliasMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        let DeviceIdentity::Platform(platform) = desc.identity() else {
            return MatchResult::NoMatch;
        };
        self.match_alias(&platform)
    }
}

impl DeviceMatcher for FirmwareMatchSpec {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        let DeviceIdentity::Platform(platform) = desc.identity() else {
            return MatchResult::NoMatch;
        };
        match (desc.origin(), platform.firmware_id, platform.alias) {
            (DiscoveryOrigin::DeviceTree, Some(id), _) if self.match_dt(id).is_some() => {
                MatchResult::Match {
                    priority: priority::EXACT,
                }
            }
            (DiscoveryOrigin::Acpi, Some(id), _) if self.match_acpi(id).is_some() => {
                MatchResult::Match {
                    priority: priority::EXACT,
                }
            }
            (DiscoveryOrigin::PlatformStatic, _, Some(name)) if name == self.alias => {
                MatchResult::Match {
                    priority: priority::EXACT,
                }
            }
            _ => MatchResult::NoMatch,
        }
    }

    fn firmware_spec(&self) -> Option<&FirmwareMatchSpec> {
        Some(self)
    }
}

/// Matcher that never matches; for drivers that are never auto-bound.
pub struct NeverMatcher;

impl DeviceMatcher for NeverMatcher {
    fn matches(&self, _desc: &DeviceDesc) -> MatchResult {
        MatchResult::NoMatch
    }
}

/// Linux-like device driver operations.
pub trait DeviceDriver: Send + Sync {
    /// Stable human-readable driver name.
    fn name(&self) -> &'static str;

    /// Subsystem kind produced by this driver.
    fn device_kind(&self) -> DeviceKind;

    /// Bus matching domains this driver participates in.
    fn bus_types(&self) -> &'static [BusTypeId];

    /// Device matcher exposed to the bus type during probe.
    fn matcher(&self) -> &dyn DeviceMatcher;

    /// Probe an unpublished device object and bind runtime state to its class.
    fn probe_device(&self, device: Arc<DeviceObject>) -> DriverResult<()>;

    /// Remove a previously probed device.
    fn remove(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }

    /// Quiesce a device for system suspend (low-power / sleep transition).
    ///
    /// The default does nothing. Drivers that own stateful hardware should
    /// save context and stop DMA/IRQ activity here. Unlike [`remove`], the
    /// device stays bound and is expected to be brought back via [`resume`].
    ///
    /// [`remove`]: DeviceDriver::remove
    /// [`resume`]: DeviceDriver::resume
    fn suspend(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }

    /// Restore a device previously quiesced by [`suspend`].
    ///
    /// The default does nothing. Drivers should re-program hardware and
    /// restore any context saved during suspend.
    ///
    /// [`suspend`]: DeviceDriver::suspend
    fn resume(&self, _device: Arc<DeviceObject>) -> DriverResult<()> {
        Ok(())
    }

    /// Bring a device to a quiescent state on system shutdown/reboot.
    ///
    /// The default does nothing. Drivers should stop ongoing activity and
    /// leave the hardware in a safe state for firmware/the next boot. Unlike
    /// [`remove`], no resources need to be released because the system is
    /// going down.
    ///
    /// [`remove`]: DeviceDriver::remove
    fn shutdown(&self, _device: Arc<DeviceObject>) {}
}

/// Registered driver object saved in the matching domain.
///
/// Owns the lifecycle-only state (id and bound device list); all immutable
/// metadata is delegated back to the underlying [`DeviceDriver`] `ops`, so
/// there is no risk of getting an out-of-sync cached copy.
pub struct DriverObject {
    id: DriverId,
    ops: Arc<dyn DeviceDriver>,
    bound_devices: SpinNoPreempt<Vec<DeviceId>>,
    probe: ProbeCounters,
}

impl DriverObject {
    /// Create a registered driver object.
    pub fn new(id: DriverId, ops: Arc<dyn DeviceDriver>) -> Self {
        Self {
            id,
            ops,
            bound_devices: SpinNoPreempt::new(Vec::new()),
            probe: ProbeCounters::new(),
        }
    }

    /// Unique driver ID.
    pub const fn id(&self) -> DriverId {
        self.id
    }

    /// Driver metadata snapshot.
    pub fn info(&self) -> DriverInfo {
        DriverInfo {
            id: self.id,
            name: self.ops.name(),
            device_kind: self.ops.device_kind(),
        }
    }

    /// Driver name.
    pub fn name(&self) -> &'static str {
        self.ops.name()
    }

    /// Produced device kind.
    pub fn device_kind(&self) -> DeviceKind {
        self.ops.device_kind()
    }

    /// Bus matching domains.
    pub fn bus_types(&self) -> &'static [BusTypeId] {
        self.ops.bus_types()
    }

    /// Runtime operations.
    pub fn ops(&self) -> Arc<dyn DeviceDriver> {
        self.ops.clone()
    }

    /// Record a bound device.
    pub(crate) fn attach_device(&self, id: DeviceId) {
        let mut devices = self.bound_devices.lock();
        if !devices.contains(&id) {
            devices.push(id);
        }
    }

    /// Drop a bound device association.
    pub(crate) fn detach_device(&self, id: DeviceId) {
        let mut devices = self.bound_devices.lock();
        if let Some(pos) = devices.iter().position(|current| *current == id) {
            devices.swap_remove(pos);
        }
    }

    /// Snapshot bound device IDs.
    pub fn bound_devices_snapshot(&self) -> Vec<DeviceId> {
        self.bound_devices.lock().clone()
    }

    /// Record one dispatched probe attempt against this driver.
    pub(crate) fn record_probe_attempt(&self) {
        self.probe.record_attempt();
    }

    /// Record one failed probe attempt against this driver.
    pub(crate) fn record_probe_failure(&self) {
        self.probe.record_failure();
    }

    /// Snapshot this driver's probe accounting.
    pub fn probe_stats(&self) -> ProbeStats {
        self.probe.snapshot()
    }
}
