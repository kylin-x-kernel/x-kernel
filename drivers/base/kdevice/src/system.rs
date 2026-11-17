// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System-device identity and lifecycle.
//!
//! System devices are the non-discoverable devices that the kernel itself
//! enumerates: CPUs and memory blocks. They have no bus, no `major/minor`
//! number, and no `/dev` node, so they cannot be represented by
//! [`crate::DeviceObject`] without inventing identity that does not exist.
//! This module therefore owns a small, independent registry keyed by
//! [`SystemDeviceId`] and shared by the hotplug policy crate and `sysfs`.
//!
//! # Lifecycle
//!
//! ```text
//!            install                      set_online(true)
//!   (absent) -------> present/offline -------------------> present/online
//!                        ^     |                                 |
//!        remove          |     | set_online(false)               |
//!                        |     v                                 |
//!                        |  present/going-offline ---------------+
//!                        |     | (control fails: back to online)
//!                        +-----+ (control succeeds: present/offline)
//! ```
//!
//! `is_present` and `is_online` are independent: an offline CPU or memory
//! block stays *present* (and therefore keeps its sysfs directory) while
//! being unusable. Removal is the only operation that deletes identity, and
//! it publishes [`SystemDeviceEventKind::Removed`] so projections such as
//! `sysfs` can drop the directories.
//!
//! # Locking and execution context
//!
//! `SYSTEM_DEVICES` is a `SpinNoPreempt`, so every function here runs in task
//! context only, matching the driver-core rule documented in the crate root.
//! The order is: registry lock first, then the per-device control callback.
//! The registry guard is always dropped before `SystemDeviceControl` runs and
//! before subscriber callbacks run, so a control implementation may call back
//! into [`system_device_state`] without deadlocking and a subscriber may
//! trigger further device-model work.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};

use kerrno::{KErrorKind, KResult};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

/// Class of a system device.
///
/// The names are the Linux `sysfs` directory names so that the projection can
/// use them verbatim.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SystemClass {
    /// A CPU described by the platform firmware (MADT/DT).
    Cpu,
    /// A memory block described by the platform firmware.
    Memory,
}

impl SystemClass {
    /// Every class, in `sysfs` order.
    pub const ALL: [SystemClass; 2] = [SystemClass::Cpu, SystemClass::Memory];

    /// Stable name of this class, as used by `/sys/devices/system/<name>`.
    pub const fn name(self) -> &'static str {
        match self {
            SystemClass::Cpu => "cpu",
            SystemClass::Memory => "memory",
        }
    }
}

/// Stable identity of a system device inside its class.
///
/// `index` is the Linux device index: the logical CPU id for
/// [`SystemClass::Cpu`] and the memory-block index ([`Self`]'s
/// `physical_start_bytes / block_size_bytes`) for [`SystemClass::Memory`].
/// Removal drops the registered identity, but the same class/index pair may
/// later be installed again as a new lifecycle. Callers must therefore treat
/// an ID as meaningful only while it is present: a stale snapshot must not be
/// used to infer that a later device with the same ID is the original one.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SystemDeviceId {
    class: SystemClass,
    index: u32,
}

impl SystemDeviceId {
    /// Creates an ID from its class and class-local index.
    pub const fn new(class: SystemClass, index: u32) -> Self {
        Self { class, index }
    }

    /// Class this device belongs to.
    pub const fn class(self) -> SystemClass {
        self.class
    }

    /// Class-local index, i.e. the `N` in `cpuN` / `memoryN`.
    pub const fn index(self) -> u32 {
        self.index
    }
}

/// Snapshot of one system device's presence and online state.
///
/// A snapshot is a value, not a live view: callers must not cache it across a
/// hotplug operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemDeviceState {
    /// Identity this state belongs to.
    pub id: SystemDeviceId,
    /// The device exists in the platform description.
    pub is_present: bool,
    /// The kernel currently uses the device.
    pub is_online: bool,
    /// An offline request is in flight; `sysfs` reports `going-offline`.
    pub is_going_offline: bool,
    /// First physical address covered by this device, in bytes.
    pub physical_start_bytes: u64,
    /// Number of bytes covered by this device.
    pub size_bytes: u64,
}

impl SystemDeviceState {
    /// Reports the `sysfs` `state` string for memory-style devices.
    pub const fn state_name(&self) -> &'static str {
        if self.is_going_offline {
            "going-offline"
        } else if self.is_online {
            "online"
        } else {
            "offline"
        }
    }
}

/// Class-specific online/offline operation.
///
/// Implementations perform the *hardware and allocator* work for one device.
/// They are called by [`set_system_device_online`] after the generic state
/// machine has validated the request and published
/// [`SystemDeviceEventKind::GoingOffline`] / [`SystemDeviceEventKind::Added`],
/// and they must either commit the requested state or leave the device
/// unchanged and return an error. They must not publish system-device events
/// themselves; the state machine owns event ordering.
pub trait SystemDeviceControl: Send + Sync {
    /// Brings this device online (`true`) or offline (`false`).
    ///
    /// Returning an error must leave the device in its previous working
    /// state; the state machine restores the published state accordingly.
    fn set_online(&self, online: bool) -> KResult<()>;
}

/// Registration record for one system device.
pub struct SystemDeviceDescriptor {
    /// Identity of the device being installed.
    pub id: SystemDeviceId,
    /// First physical address covered by the device, in bytes.
    pub physical_start_bytes: u64,
    /// Number of bytes covered by the device.
    pub size_bytes: u64,
    /// Whether the device is usable right after installation.
    pub is_online: bool,
    /// Class-specific online/offline implementation.
    pub control: Arc<dyn SystemDeviceControl>,
}

impl SystemDeviceDescriptor {
    /// Builds a descriptor.
    pub fn new(
        id: SystemDeviceId,
        physical_start_bytes: u64,
        size_bytes: u64,
        is_online: bool,
        control: Arc<dyn SystemDeviceControl>,
    ) -> Self {
        Self {
            id,
            physical_start_bytes,
            size_bytes,
            is_online,
            control,
        }
    }
}

/// Kind of system-device lifecycle transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemDeviceEventKind {
    /// The device became present; its directory must appear.
    Added,
    /// The device is no longer present; its directory must disappear.
    Removed,
    /// An offline request was accepted and is being applied.
    GoingOffline,
    /// The device is online (published after a successful online request and
    /// as the restore notification after a failed offline request).
    Online,
    /// The device is offline (published after a successful offline request and
    /// as the restore notification after a failed online request).
    Offline,
}

/// One system-device lifecycle notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SystemDeviceEvent {
    /// Transition that happened.
    pub kind: SystemDeviceEventKind,
    /// State *after* the transition. For [`SystemDeviceEventKind::Removed`]
    /// this is the last state, with `is_present == false`.
    pub state: SystemDeviceState,
}

/// Callback invoked for every system-device lifecycle transition.
pub type SystemDeviceEventCallback = Arc<dyn Fn(SystemDeviceEvent) + Send + Sync>;

struct SystemDeviceEntry {
    state: SystemDeviceState,
    control: Arc<dyn SystemDeviceControl>,
    /// Set while [`SystemDeviceControl::set_online`] is running so that two
    /// concurrent requests cannot both drive the same hardware transition.
    is_transitioning: bool,
}

struct SystemDeviceRegistry {
    devices: BTreeMap<SystemDeviceId, SystemDeviceEntry>,
    subscribers: Vec<SystemDeviceEventCallback>,
}

impl SystemDeviceRegistry {
    fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
            subscribers: Vec::new(),
        }
    }
}

static SYSTEM_DEVICES: LazyInit<SpinNoPreempt<SystemDeviceRegistry>> = LazyInit::new();

fn with_registry<R>(f: impl FnOnce(&mut SystemDeviceRegistry) -> R) -> R {
    SYSTEM_DEVICES.call_once(|| SpinNoPreempt::new(SystemDeviceRegistry::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed successfully and \
                  permanently published the system-device registry"
    )]
    let registry = SYSTEM_DEVICES
        .get()
        .expect("system-device registry call_once succeeded");
    f(&mut registry.lock())
}

fn subscribers_snapshot(registry: &SystemDeviceRegistry) -> Vec<SystemDeviceEventCallback> {
    registry.subscribers.clone()
}

fn publish(subscribers: &[SystemDeviceEventCallback], event: SystemDeviceEvent) {
    for subscriber in subscribers {
        subscriber(event);
    }
}

/// Installs a present system device.
///
/// Returns `EEXIST` when `descriptor.id` is already installed. The `Added`
/// event is published after the registry has been updated and its lock
/// released.
pub fn install_system_device(descriptor: SystemDeviceDescriptor) -> KResult<()> {
    let state = SystemDeviceState {
        id: descriptor.id,
        is_present: true,
        is_online: descriptor.is_online,
        is_going_offline: false,
        physical_start_bytes: descriptor.physical_start_bytes,
        size_bytes: descriptor.size_bytes,
    };
    let (subscribers, entry) = with_registry(|registry| {
        if registry.devices.contains_key(&descriptor.id) {
            return (Vec::new(), None);
        }
        registry.devices.insert(
            descriptor.id,
            SystemDeviceEntry {
                state,
                control: descriptor.control,
                is_transitioning: false,
            },
        );
        (subscribers_snapshot(registry), Some(state))
    });
    match entry {
        Some(state) => {
            publish(
                &subscribers,
                SystemDeviceEvent {
                    kind: SystemDeviceEventKind::Added,
                    state,
                },
            );
            Ok(())
        }
        None => Err(KErrorKind::AlreadyExists.into()),
    }
}

/// Removes a system device.
///
/// The device disappears from [`system_device_snapshot`] and its `Removed`
/// event carries `is_present == false`. A removal request during an in-flight
/// transition is rejected with `ENODEV`; this prevents the control callback
/// from running against an identity that another task removed. Returns
/// `ENODEV` for an unknown device.
pub fn remove_system_device(id: SystemDeviceId) -> KResult<()> {
    let (subscribers, removed_state) = with_registry(|registry| {
        let Some(entry) = registry.devices.get(&id) else {
            return (Vec::new(), None);
        };
        if entry.is_transitioning {
            return (Vec::new(), None);
        }
        let mut state = entry.state;
        state.is_present = false;
        state.is_going_offline = false;
        registry.devices.remove(&id);
        (subscribers_snapshot(registry), Some(state))
    });
    match removed_state {
        Some(state) => {
            publish(
                &subscribers,
                SystemDeviceEvent {
                    kind: SystemDeviceEventKind::Removed,
                    state,
                },
            );
            Ok(())
        }
        None => Err(KErrorKind::NoSuchDevice.into()),
    }
}

/// Registers a lifecycle subscriber and returns the current device snapshot.
///
/// The subscriber is installed and the snapshot is captured while the registry
/// lock is held. Callbacks therefore cannot overtake the snapshot: an event
/// published after this call is reflected by either the snapshot or a later
/// callback, never lost between them.
///
/// Callbacks run outside the registry lock, in registration order, and must
/// not block. Registration is permanent for the lifetime of the kernel; the
/// sysfs projection registers once during device-projection initialization.
pub fn subscribe_system_device_events(
    callback: SystemDeviceEventCallback,
) -> Vec<SystemDeviceState> {
    with_registry(|registry| {
        registry.subscribers.push(callback);
        registry.devices.values().map(|entry| entry.state).collect()
    })
}

/// Returns the current state of one system device, or `None` if it is absent.
pub fn system_device_state(id: SystemDeviceId) -> Option<SystemDeviceState> {
    with_registry(|registry| registry.devices.get(&id).map(|entry| entry.state))
}

/// Returns every installed system device, ordered by class then index.
pub fn system_device_snapshot() -> Vec<SystemDeviceState> {
    with_registry(|registry| registry.devices.values().map(|entry| entry.state).collect())
}

/// Returns every installed device of `class`, ordered by index.
pub fn present_system_devices(class: SystemClass) -> Vec<SystemDeviceId> {
    with_registry(|registry| {
        registry
            .devices
            .values()
            .filter(|entry| entry.state.id.class == class)
            .map(|entry| entry.state.id)
            .collect()
    })
}

/// Drives the generic online/offline state machine for one device.
///
/// The sequence is fixed and is the only place that publishes
/// `GoingOffline`/`Online`/`Offline`:
///
/// 1. validate the request against the current state (`ENODEV` if absent,
///    success if already in the requested state, `EBUSY` if another request
///    is in flight);
/// 2. for an offline request, set `is_going_offline` and publish
///    `GoingOffline` before touching hardware;
/// 3. run [`SystemDeviceControl::set_online`] with no registry lock held;
/// 4. commit the new state and publish `Offline`/`Online`, or restore the
///    previous state and publish the restoring event.
pub fn set_system_device_online(id: SystemDeviceId, online: bool) -> KResult<()> {
    let prepared = with_registry(|registry| {
        let Some(entry) = registry.devices.get_mut(&id) else {
            return Err(KErrorKind::NoSuchDevice);
        };
        if entry.is_transitioning {
            return Err(KErrorKind::ResourceBusy);
        }
        if entry.state.is_online == online && !entry.state.is_going_offline {
            // Already in the requested state: an idempotent success, not an
            // error, so a retrying caller does not see spurious failures.
            return Ok(None);
        }
        let was_online = entry.state.is_online;
        entry.is_transitioning = true;
        if !online {
            entry.state.is_going_offline = true;
        }
        Ok(Some((
            entry.control.clone(),
            was_online,
            subscribers_snapshot(registry),
        )))
    })?;
    let Some((control, was_online, subscribers)) = prepared else {
        return Ok(());
    };

    if !online {
        let state = with_registry(|registry| registry.devices.get(&id).map(|entry| entry.state));
        if let Some(state) = state {
            publish(
                &subscribers,
                SystemDeviceEvent {
                    kind: SystemDeviceEventKind::GoingOffline,
                    state,
                },
            );
        }
    }

    let result = control.set_online(online);

    let (event_kind, state) = with_registry(|registry| {
        let Some(entry) = registry.devices.get_mut(&id) else {
            return (None, None);
        };
        entry.is_transitioning = false;
        entry.state.is_going_offline = false;
        match result {
            Ok(()) => {
                entry.state.is_online = online;
                let kind = if online {
                    SystemDeviceEventKind::Online
                } else {
                    SystemDeviceEventKind::Offline
                };
                (Some(kind), Some(entry.state))
            }
            Err(_) => {
                entry.state.is_online = was_online;
                let kind = if was_online {
                    SystemDeviceEventKind::Online
                } else {
                    SystemDeviceEventKind::Offline
                };
                (Some(kind), Some(entry.state))
            }
        }
    });

    if let (Some(kind), Some(state)) = (event_kind, state) {
        publish(&subscribers, SystemDeviceEvent { kind, state });
    }
    result
}

/// Clears the system-device registry.
///
/// Only for unit tests: it drops subscriber registrations and installed
/// devices so each test starts from a known state.
#[cfg(unittest)]
pub fn reset_system_devices_for_tests() {
    with_registry(|registry| *registry = SystemDeviceRegistry::new());
}

unittest::path_mod_test!(tests, "../ut-tests/system_device.rs");
