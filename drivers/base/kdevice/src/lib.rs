// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device core for x-kernel.
//!
//! This crate hosts the shared device model and owns the device registry,
//! lifecycle orchestration, binding/removal serialization and topology
//! snapshots. It is framework core rather than a pure interface crate:
//! `kdriver` and `kclass` consume it, and it does not depend on the driver
//! orchestration layer.
//! It also owns stable Linux-facing device-node identity: class index,
//! major/minor number, `devname`, and the immutable metadata projected by
//! `devfs` and `sysfs`.
//!
//! # Lock-order discipline
//!
//! Driver-core uses one sleepable lock per discovery descriptor plus several
//! spinlock-protected objects: the registry (`DeviceRegistry`), bus instances
//! (`BusInstance`), bus types (`BusTypeObject`), driver objects
//! (`DriverObject`), and per-object leaf locks inside `DeviceObject`
//! (`slot`, `relations`, plus the current binding's `meta` and `devres`). To
//! prevent deadlocks, all code paths that need to touch more than one of these
//! must observe the following order:
//!
//! 1. **Descriptor binding lock first**. Each descriptor and its stable device
//!    object share one sleepable `kmutex` that serializes binding install/
//!    clear, cleanup registration, removal and topology entry points. It is
//!    taken outside every spinlock (blocking acquisition would otherwise
//!    disable preemption for an unbounded time); holding it may then take the
//!    registry and per-object locks. No path may acquire it while holding a
//!    spinlock, and only its holder may set or clear the descriptor's
//!    `Probing` state or the object's current binding. Registration uses
//!    `try_lock` on the still-unpublished descriptor lock so enumeration never
//!    sleeps.
//! 2. **Registry next**: take the registry guard, snapshot the `Arc` handles
//!    you need, then drop the registry guard before reaching into a
//!    bus-instance lock that can block or nest further. Snapshot helpers that
//!    build device records do this in two phases: collect objects under the
//!    registry lock, read their leaves after releasing it.
//! 3. **Per-object order** is `bus.devices` -> `bus.drivers` and
//!    `bus.devices` -> one `DeviceObject` leaf lock. The leaves (`slot`,
//!    `relations`, `binding.meta`, `binding.devres`) are never held together.
//!    A topology write may hold several `relations` locks at once, but only by
//!    acquiring them in strictly ascending `DeviceId` order, independent of
//!    parent/child role. There is deliberately no path from a `DeviceObject`
//!    lock back into the registry or a bus instance, except the documented
//!    snapshot order `registry -> relations` used while unregistering.
//! 4. **Never re-enter the registry from inside a per-object lock**. Per-
//!    object methods (`BusInstance::attach_bound_device`,
//!    `BusTypeObject::*`, `DriverObject::*`, `DeviceObject::*`) must not call
//!    back into `device_registry()`.
//! 5. **Lifecycle subscriber callbacks** run outside the registry and all
//!    per-object spinlocks, but they may still run while the emitting path
//!    holds the descriptor binding lock (probe, adoption, registration scan,
//!    removal). Callbacks must not call any driver-core mutator
//!    (`probe_device_desc`, `register_driver_object`, `remove_device_*`,
//!    `unregister_device_desc`, etc.) on the same thread: probing the same
//!    descriptor returns `Busy`, while paths that block, such as removal,
//!    would self-deadlock.
//!    Subscribers should snapshot what they need (e.g. via
//!    `find_device(id)`) and defer non-trivial work.
//!
//! # Execution context
//!
//! The descriptor binding lock is a `kmutex` and requires sleepable task
//! context; waiting on it may block the calling task. All other driver-core
//! locks are **preempt-disabling** spinlocks (`kspin::SpinNoPreempt`), not
//! IRQ-disabling ones. They keep the current task from being preempted while
//! held, but they do **not** mask interrupts. Consequently none of the
//! driver-core mutators or registry accessors are safe to call from hard-IRQ
//! context: an interrupt taken while a CPU holds one of these locks could
//! re-enter the same lock and self-deadlock.
//!
//! Therefore:
//!
//! - Call driver-core APIs (`probe_device_desc`, `remove_device_*`, registry
//!   lookups, `DeviceObject`/`BusInstance`/`DriverObject` mutators, lifecycle
//!   dispatch) only from task/thread context.
//! - `probe_device_desc` reports `ProbeOutcome::Busy` only for same-context
//!   recursion (checked with `kmutex::Mutex::is_owned_by_current`). A
//!   different task blocks on the binding lock and rechecks the state instead.
//! - IRQ handlers must stay off these paths. An interrupt handler should only
//!   acknowledge the device and wake the consumer/task that will then perform
//!   any device-model work outside IRQ context.

#![no_std]

extern crate alloc;

pub mod bus;
pub mod device;
mod device_node;
pub mod driver;
mod lifecycle;
mod registry;
pub mod system;
pub mod topology;

pub use bus::{
    BusId, BusInfo, BusInstance, BusType, BusTypeId, BusTypeObject, DriverMatchInfo,
    PciBusTypeMatcher, PlatformBusTypeMatcher,
};
pub use device::{
    BindingRef,
    desc::{
        DeviceDesc, DeviceDescId, DeviceDescSpec, DeviceId, DeviceIdentity, DeviceLocation,
        DeviceNumber, DeviceRecord, DeviceState, DiscoveryOrigin, IntxRouteStatus, PciIdentity,
        PlatformIdentity, TransportInfo,
    },
    handles::{BusHandle, DeviceCore, DriverCore},
    object::{DeviceObject, DeviceUse},
    resource::{
        DmaSpec, IoPortRange, IrqResource, IrqTrigger, MmioRegion, ResourceDesc, ResourceSet,
    },
};
pub use device_node::{
    DRM_RENDER_MINOR_BASE, DeferredNodeRetirement, DeviceNode, DeviceNodeCallback, DeviceNodeClass,
    DeviceNodeEvent, DeviceNodeKind, DeviceNodeLifecycleCallback, DeviceNodeLifecycleEvent,
    DrmDeviceNodeMetadata, InputDeviceNodeId, InputDeviceNodeMetadata, NodeRemoval,
    device_nodes_snapshot, install_device_node_lifecycle_publisher, register_device_node,
    register_device_node_for_binding, subscribe_device_nodes, unregister_device_node,
    unregister_device_node_for_binding,
};
pub use driver::{
    BoxedDriver, CompatibleAliasMatcher, DeviceDriver, DeviceMatcher, DriverId, DriverInfo,
    DriverObject, FirmwareMatchSpec, MatchResult, NeverMatcher, PciDeviceId, PciIdsMatcher,
    PmContext, ProbeContext, ProbeStats, RemoveContext, VirtioTypeMatcher,
};
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
#[cfg(unittest)]
pub use lifecycle::dispatch::dispatch_device_event_for_tests;
#[cfg(unittest)]
pub use lifecycle::test_hooks;
pub use lifecycle::{
    ActiveDeviceAdoption, ProbeOutcome, adopt_active_device, attach_device_parent,
    detach_device_parent, device_desc_add,
    dispatch::{subscribe_device_event_kind, subscribe_device_removed},
    event::{DeviceEvent, DeviceEventKind},
    probe_device_desc, register_bus_instance, register_driver_object, remove_device_managed,
    unregister_device_desc,
};
pub(crate) use registry::device_registry;
pub use registry::{
    bus_infos_snapshot, device_descs_snapshot, device_records_snapshot, driver_infos_snapshot,
    find_bus, find_bus_by_name, find_device, find_device_desc, find_driver, find_driver_by_name,
    firmware_match_specs_for_bus_type, init_device_registry, register_bus_type,
};
#[cfg(unittest)]
pub use registry::{reset_and_setup_platform_bus_for_tests, reset_device_registry_for_tests};
#[cfg(unittest)]
pub use system::reset_system_devices_for_tests;
pub use system::{
    SystemClass, SystemDeviceControl, SystemDeviceDescriptor, SystemDeviceEvent,
    SystemDeviceEventCallback, SystemDeviceEventKind, SystemDeviceId, SystemDeviceState,
    install_system_device, present_system_devices, remove_system_device, set_system_device_online,
    subscribe_system_device_events, system_device_snapshot, system_device_state,
};
pub use topology::{BusView, DeviceCoreView, DeviceTopology, DriverCoreView, device_topology};
