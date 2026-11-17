// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver identity and driver-core objects shared by the driver framework.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use driver_base::{DeviceKind, DriverResult};
use kspin::SpinNoPreempt;

use crate::{
    BusTypeId, DeviceDesc, DeviceId, DeviceIdentity, DiscoveryOrigin, TransportInfo,
    device::desc::PlatformIdentity,
};

mod context;
mod data;
mod erased;

pub(crate) use context::ContextPermit;
pub use context::{PmContext, ProbeContext, RemoveContext};
pub(crate) use data::DriverData;
pub use erased::BoxedDriver;
pub(crate) use erased::ErasedCallError;

/// Probe accounting for a driver or bus instance.
///
/// Snapshotted via [`DriverObject::probe_stats`] and
/// [`crate::BusInstance::probe_stats`] to help diagnose drivers or buses
/// with repeated probe failures (see review item D2).
///
/// Counters are cumulative for the lifetime of the object and are diagnostics
/// only: they never suppress a candidate, blacklist a driver, or prevent a
/// later retry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DriverId(u64);

impl DriverId {
    /// Wraps a raw numeric driver id.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Persistent driver metadata.
#[derive(Clone, Debug)]
pub struct DriverInfo {
    /// Unique driver ID.
    pub id: DriverId,
    /// Human-readable driver name.
    pub name: &'static str,
    /// The device subsystem this driver serves.
    pub device_kind: DeviceKind,
}

/// Result of a driver's match attempt against a discovery descriptor.
///
/// Matching is no longer scored. Candidates are evaluated in registration
/// order and the first successful probe wins, so a matcher only reports
/// whether the driver supports the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchResult {
    /// The driver does not support this device.
    NoMatch,
    /// The driver can handle this device.
    Match,
}

/// PCI vendor/device ID pair for matching.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PciDeviceId {
    /// PCI vendor id to match against.
    pub vendor_id: u16,
    /// PCI device id to match against.
    pub device_id: u16,
}

/// Firmware identity table for one platform driver family.
///
/// The `alias` is the stable kernel-side identity used by platform-static
/// fallback descriptors. Raw firmware identifiers stay split by source so DT,
/// ACPI, and static fallbacks do not accidentally match each other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
            MatchResult::Match
        } else {
            MatchResult::NoMatch
        }
    }
}

/// Match VirtIO transport (PCI or MMIO) by VirtIO device type code.
pub struct VirtioTypeMatcher {
    /// VirtIO device type code this matcher accepts (1 = net, 2 = block, ...).
    pub device_type: u32,
}

impl DeviceMatcher for VirtioTypeMatcher {
    fn matches(&self, desc: &DeviceDesc) -> MatchResult {
        match desc.transport() {
            Some(TransportInfo::Virtio { device_type }) if device_type == self.device_type => {
                MatchResult::Match
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
            MatchResult::Match
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
                MatchResult::Match
            }
            (DiscoveryOrigin::Acpi, Some(id), _) if self.match_acpi(id).is_some() => {
                MatchResult::Match
            }
            (DiscoveryOrigin::PlatformStatic, _, Some(name)) if name == self.alias => {
                MatchResult::Match
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
///
/// `Data` is owned by one binding runtime for the
/// lifetime of one binding. `probe_device` returns it; the framework stores it
/// and passes the same instance to `remove`/PM callbacks. Drivers never see
/// the type-erased storage or an `Any` getter.
pub trait DeviceDriver: Send + Sync {
    /// Private state of one binding. `Send` is required because the binding
    /// may be retired from another task; `Sync` is deliberately not required.
    type Data: Send + 'static;

    /// Stable human-readable driver name.
    fn name(&self) -> &'static str;

    /// Subsystem kind produced by this driver.
    fn device_kind(&self) -> DeviceKind;

    /// Bus matching domains this driver participates in.
    fn bus_types(&self) -> &'static [BusTypeId];

    /// Device matcher exposed to the bus type during probe.
    fn matcher(&self) -> &dyn DeviceMatcher;

    /// Probe an unpublished device and return this binding's private state.
    fn probe_device(&self, ctx: &ProbeContext<'_>) -> DriverResult<Self::Data>;

    /// Remove a previously probed device.
    ///
    /// Returning `Ok` or `Err` both require the device to be stopped and no
    /// longer DMA/IRQ accessible before returning. `Err` reports a non-fatal
    /// post-stop cleanup error; the framework logs it and continues teardown.
    /// Implementations that cannot stop must not return through this path.
    fn remove(&self, _ctx: &RemoveContext<'_>, _data: Option<&mut Self::Data>) -> DriverResult<()> {
        Ok(())
    }

    /// Quiesce a device for system suspend. PM scheduling is not wired yet.
    fn suspend(&self, _ctx: &PmContext<'_>, _data: Option<&mut Self::Data>) -> DriverResult<()> {
        Ok(())
    }

    /// Restore a device previously quiesced by [`suspend`].
    fn resume(&self, _ctx: &PmContext<'_>, _data: Option<&mut Self::Data>) -> DriverResult<()> {
        Ok(())
    }

    /// Bring a device to a quiescent state on shutdown/reboot.
    fn shutdown(&self, _ctx: &PmContext<'_>, _data: Option<&mut Self::Data>) {}
}

/// Registered driver object saved in the matching domain.
///
/// Owns the lifecycle-only state (id and bound device list); all immutable
/// metadata is delegated back to the underlying [`DeviceDriver`] `ops`, so
/// there is no risk of getting an out-of-sync cached copy.
pub struct DriverObject {
    id: DriverId,
    ops: BoxedDriver,
    bound_devices: SpinNoPreempt<Vec<DeviceId>>,
    probe: ProbeCounters,
}

impl DriverObject {
    /// Create a registered driver object.
    pub fn new(id: DriverId, ops: BoxedDriver) -> Self {
        Self {
            id,
            ops,
            bound_devices: SpinNoPreempt::new(Vec::new()),
            probe: ProbeCounters::new(),
        }
    }

    pub(crate) fn probe_erased(&self, ctx: &ProbeContext<'_>) -> DriverResult<DriverData> {
        self.ops.erased().probe(ctx)
    }

    pub(crate) fn remove_erased(
        &self,
        ctx: &RemoveContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        self.ops.erased().remove(ctx, data)
    }

    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are reserved and tested only in unit \
                      builds"
        )
    )]
    pub(crate) fn suspend_erased(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        self.ops.erased().suspend(ctx, data)
    }

    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are reserved and tested only in unit \
                      builds"
        )
    )]
    pub(crate) fn resume_erased(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        self.ops.erased().resume(ctx, data)
    }

    #[cfg_attr(
        all(not(unittest), not(doc)),
        expect(
            dead_code,
            reason = "CG-024-lint-exceptions: PM callbacks are reserved and tested only in unit \
                      builds"
        )
    )]
    pub(crate) fn shutdown_erased(
        &self,
        ctx: &PmContext<'_>,
        data: Option<&mut DriverData>,
    ) -> Result<(), ErasedCallError> {
        self.ops.erased().shutdown(ctx, data)
    }

    /// Matcher metadata exposed to bus types.
    pub fn matcher(&self) -> &dyn DeviceMatcher {
        self.ops.erased().matcher()
    }

    pub fn supports_adoption(&self) -> bool {
        self.ops.supports_adoption()
    }

    /// Unique driver ID.
    pub const fn id(&self) -> DriverId {
        self.id
    }

    /// Driver metadata snapshot.
    pub fn info(&self) -> DriverInfo {
        DriverInfo {
            id: self.id,
            name: self.ops.erased().name(),
            device_kind: self.ops.erased().device_kind(),
        }
    }

    /// Driver name.
    pub fn name(&self) -> &'static str {
        self.ops.erased().name()
    }

    /// Produced device kind.
    pub fn device_kind(&self) -> DeviceKind {
        self.ops.erased().device_kind()
    }

    /// Bus matching domains.
    pub fn bus_types(&self) -> &'static [BusTypeId] {
        self.ops.erased().bus_types()
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
