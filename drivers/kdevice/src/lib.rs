// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common device model types for x-kernel.
//!
//! This crate hosts stable, shared device-model concepts that should be
//! reusable by `kdriver` and higher-level subsystems without forcing them to
//! depend on the driver orchestration layer.
//!
//! # Lock-order discipline
//!
//! Driver-core uses several spinlock-protected objects: the registry
//! (`DeviceRegistry`), bus instances (`BusInstance`), bus types
//! (`BusTypeObject`), driver objects (`DriverObject`), and per-device state
//! inside `DeviceObject`. To prevent deadlocks, all code paths that need to
//! touch more than one of these must observe the following order:
//!
//! 1. **Registry first**: take the registry guard, take/snapshot the `Arc`
//!    handles you need, then drop the registry guard before reaching into
//!    any per-object lock.
//! 2. **Never re-enter the registry from inside a per-object lock**. Per-
//!    object methods (`BusInstance::add_device`, `BusTypeObject::*`,
//!    `DriverObject::*`, `DeviceObject::*`) must not call back into
//!    `device_registry()`.
//! 3. **Lifecycle subscriber callbacks** run outside every driver-core lock.
//!    Callbacks must not call back into `device_registry()` or any
//!    driver-core mutator (`probe_device_desc`, `remove_device_*`, etc.) on
//!    the same thread; doing so risks reentrant locking and ordering loops.
//!    Subscribers should snapshot what they need (e.g. via
//!    `find_device(id)`) and defer non-trivial work.
//!
//! # Execution context
//!
//! Every driver-core lock above is a **preempt-disabling** spinlock
//! (`kspin::SpinNoPreempt`), not an IRQ-disabling one. It keeps the current
//! task from being preempted while held, but it does **not** mask interrupts.
//! Consequently none of the driver-core mutators or registry accessors are
//! safe to call from hard-IRQ context: an interrupt taken while a CPU holds
//! one of these locks could re-enter the same lock and self-deadlock.
//!
//! Therefore:
//!
//! - Call driver-core APIs (`probe_device_desc`, `remove_device_*`, registry
//!   lookups, `DeviceObject`/`BusInstance`/`DriverObject` mutators, lifecycle
//!   dispatch) only from task/thread context.
//! - IRQ handlers must stay off these paths. An interrupt handler should only
//!   acknowledge the device and wake the consumer/task that will then perform
//!   any device-model work outside IRQ context.

#![no_std]

extern crate alloc;

pub mod bus;
pub mod device;
pub mod driver;
mod lifecycle;
mod registry;
pub mod topology;

pub use bus::{
    BusId, BusInfo, BusInstance, BusType, BusTypeId, BusTypeObject, PciBusTypeMatcher,
    PlatformBusTypeMatcher,
};
pub use device::{
    desc::{
        DeviceDesc, DeviceDescId, DeviceId, DeviceIdentity, DeviceLocation, DeviceRecord,
        DeviceState, DiscoveryOrigin, PciIdentity, PlatformIdentity, TransportInfo,
    },
    handles::{BusHandle, DeviceCore, DriverCore},
    object::{DeviceObject, DeviceUse},
    resource::{
        DmaSpec, IoPortRange, IrqResource, IrqTrigger, MmioRegion, ResourceDesc, ResourceSet,
        irq_trigger_from_firmware,
    },
};
pub use driver::{
    CompatibleAliasMatcher, DeviceDriver, DeviceMatcher, DriverId, DriverInfo, DriverObject,
    FirmwareMatchSpec, MatchResult, NeverMatcher, PciDeviceId, PciIdsMatcher, ProbeStats,
    VirtioTypeMatcher, priority,
};
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use lifecycle::{
    ActiveDeviceAdoption, ProbeOutcome, adopt_active_device, attach_device_parent,
    detach_device_parent, device_desc_add, device_desc_add_with_parent,
    dispatch::{subscribe_device_event_kind, subscribe_device_removed},
    event::{DeviceEvent, DeviceEventKind},
    probe_device_desc, register_bus_instance, register_driver_object, remove_device_managed,
};
pub(crate) use registry::device_registry;
pub use registry::{
    bus_infos_snapshot, device_descs_snapshot, device_records_snapshot, driver_infos_snapshot,
    find_bus, find_bus_by_name, find_device, find_device_desc, find_driver, find_driver_by_name,
    firmware_match_specs_for_bus_type, init_device_registry, register_bus_type,
};
#[cfg(unittest)]
pub use registry::{reset_and_setup_platform_bus_for_tests, reset_device_registry_for_tests};
pub use topology::{BusView, DeviceCoreView, DeviceTopology, DriverCoreView, device_topology};
