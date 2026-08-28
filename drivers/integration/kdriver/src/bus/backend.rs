// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bus backend abstraction.
//!
//! Each bus instance kind (PCI, platform) implements [`BusBackend`] to
//! enumerate devices into the driver core. Multiple backends
//! can coexist at runtime, replacing the previous compile-time bus selection.

use driver_base::DriverResult;
use kdevice::{BusId, BusTypeId};

use crate::enumeration::EnumerationContext;

/// A bus backend that can enumerate devices.
///
/// Implementations call into the shared device core through [`EnumerationContext`]
/// so discovery emits `DeviceDesc`s; the driver core publishes `DeviceObject`s
/// only after descriptor probe succeeds.
pub trait BusBackend: Send {
    /// Human-readable name of this bus instance (e.g. `"pci"`,
    /// `"platform"`).
    fn name(&self) -> &'static str;

    /// The bus type identifier for driver matching.
    fn bus_type_id(&self) -> BusTypeId;

    /// Optional one-time setup before the first enumeration pass.
    fn early_init(&mut self) -> DriverResult {
        Ok(())
    }

    /// Enumerate devices on this bus and register them in `context`.
    fn enumerate(&mut self, context: &mut EnumerationContext, bus_id: BusId) -> DriverResult;

    /// Re-scan this bus for hot-plugged or removed devices.
    ///
    /// Default behaviour re-runs [`BusBackend::enumerate`]; backends that can
    /// distinguish new arrivals from already-published devices should override
    /// this hook to avoid duplicate descriptors.
    fn rescan(&mut self, context: &mut EnumerationContext, bus_id: BusId) -> DriverResult {
        self.enumerate(context, bus_id)
    }

    /// Tear down this bus instance.
    ///
    /// Default is a no-op so backends opt in to remove support incrementally.
    /// A complete implementation removes every device the backend published,
    /// releases any bus-level resources, and leaves the backend in a state
    /// where [`BusBackend::enumerate`] could be called again from scratch.
    fn remove(&mut self, _bus_id: BusId) -> DriverResult {
        Ok(())
    }

    /// Stop the bus from producing new events (interrupts, hot-plug, etc.).
    ///
    /// Default is a no-op. Backends that drive real hardware controllers
    /// should override this to mask controller interrupts and pause any
    /// background scanners before system shutdown or suspend.
    fn quiesce(&mut self, _bus_id: BusId) -> DriverResult {
        Ok(())
    }
}
