// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible device-node identities shared by filesystem projections.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    format,
    string::String,
    sync::Arc,
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, Ordering};

use driver_base::{DriverError, DriverResult};
use kspin::{SpinNoPreempt, SpinNoPreemptGuard};
use lazyinit::LazyInit;

use crate::{DeviceId, DeviceLocation, DeviceNumber};

const INPUT_MAJOR: u32 = 13;
const INPUT_EVENT_MINOR_BASE: u32 = 64;
const INPUT_EVENT_COUNT: u32 = 32;
const DRM_MAJOR: u32 = 226;
const DRM_PRIMARY_COUNT: u32 = 64;

/// Device-node class projected by devtmpfs and sysfs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceNodeKind {
    /// Linux evdev character device.
    Input,
    /// DRM primary character device.
    Drm,
}

impl DeviceNodeKind {
    fn index_count(self) -> u32 {
        match self {
            Self::Input => INPUT_EVENT_COUNT,
            Self::Drm => DRM_PRIMARY_COUNT,
        }
    }

    fn device_number(self, index: u32) -> DeviceNumber {
        match self {
            Self::Input => DeviceNumber::new(INPUT_MAJOR, INPUT_EVENT_MINOR_BASE + index),
            Self::Drm => DeviceNumber::new(DRM_MAJOR, index),
        }
    }

    fn devname(self, index: u32) -> String {
        match self {
            Self::Input => format!("input/event{index}"),
            Self::Drm => format!("dri/card{index}"),
        }
    }
}

/// Linux input identity tuple captured when an input class device is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputDeviceNodeId {
    /// Bus type identifier.
    pub bus_type: u16,
    /// Vendor identifier.
    pub vendor: u16,
    /// Product identifier.
    pub product: u16,
    /// Device version identifier.
    pub version: u16,
}

impl InputDeviceNodeId {
    /// Creates an input identity tuple.
    pub const fn new(bus_type: u16, vendor: u16, product: u16, version: u16) -> Self {
        Self {
            bus_type,
            vendor,
            product,
            version,
        }
    }
}

/// Input identity and capability data used by sysfs projection consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceNodeMetadata {
    name: String,
    physical_location: String,
    unique_id: String,
    device_id: InputDeviceNodeId,
    event_types: Vec<u8>,
    event_codes: BTreeMap<u8, Vec<u8>>,
}

impl InputDeviceNodeMetadata {
    /// Creates a snapshot of input identity and capability data.
    pub fn new(
        name: String,
        physical_location: String,
        unique_id: String,
        device_id: InputDeviceNodeId,
        event_types: Vec<u8>,
        event_codes: BTreeMap<u8, Vec<u8>>,
    ) -> Self {
        Self {
            name,
            physical_location,
            unique_id,
            device_id,
            event_types,
            event_codes,
        }
    }

    /// Driver-provided input device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Physical connection description.
    pub fn physical_location(&self) -> &str {
        &self.physical_location
    }

    /// Driver-provided unique identifier.
    pub fn unique_id(&self) -> &str {
        &self.unique_id
    }

    /// Linux input identity tuple.
    pub const fn device_id(&self) -> InputDeviceNodeId {
        self.device_id
    }

    /// Bitmap of supported event types.
    pub fn event_types(&self) -> &[u8] {
        &self.event_types
    }

    /// Bitmap of supported codes for one event type.
    pub fn event_codes(&self, event_type: u8) -> Option<&[u8]> {
        self.event_codes.get(&event_type).map(Vec::as_slice)
    }
}

/// Class-specific facts supplied when a runtime device publishes its node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNodeClass {
    /// Input device with identity and capability metadata.
    Input(InputDeviceNodeMetadata),
    /// DRM primary device.
    Drm,
}

impl DeviceNodeClass {
    const fn kind(&self) -> DeviceNodeKind {
        match self {
            Self::Input(_) => DeviceNodeKind::Input,
            Self::Drm => DeviceNodeKind::Drm,
        }
    }
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceNodeState {
    Publishing    = 0,
    Published     = 1,
    RemovePending = 2,
    Removing      = 3,
    Removed       = 4,
}

#[derive(Debug)]
struct DeviceNodeLifecycle {
    state: AtomicU8,
}

impl DeviceNodeLifecycle {
    const fn new() -> Self {
        Self {
            state: AtomicU8::new(DeviceNodeState::Publishing as u8),
        }
    }

    fn state(&self) -> DeviceNodeState {
        match self.state.load(Ordering::Acquire) {
            0 => DeviceNodeState::Publishing,
            1 => DeviceNodeState::Published,
            2 => DeviceNodeState::RemovePending,
            3 => DeviceNodeState::Removing,
            4 => DeviceNodeState::Removed,
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: The lifecycle byte is written \
                          only from DeviceNodeState discriminants, so every stored value decodes \
                          to a state"
            )]
            state => panic!("invalid device-node lifecycle state: {state}"),
        }
    }

    fn is_available(&self) -> bool {
        matches!(
            self.state(),
            DeviceNodeState::Publishing | DeviceNodeState::Published
        )
    }

    fn request_removal(&self) -> bool {
        let (current, next, should_dispatch) = match self.state() {
            DeviceNodeState::Publishing => (
                DeviceNodeState::Publishing,
                DeviceNodeState::RemovePending,
                false,
            ),
            DeviceNodeState::Published => {
                (DeviceNodeState::Published, DeviceNodeState::Removing, true)
            }
            DeviceNodeState::RemovePending
            | DeviceNodeState::Removing
            | DeviceNodeState::Removed => return false,
        };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The device registry serializes state \
                      transitions, so the previously read state cannot change before \
                      compare_exchange"
        )]
        self.state
            .compare_exchange(
                current as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("device-node state transitions are serialized by the registry");
        should_dispatch
    }

    fn finish_publishing(&self) -> bool {
        let (current, next, should_remove) = match self.state() {
            DeviceNodeState::Publishing => (
                DeviceNodeState::Publishing,
                DeviceNodeState::Published,
                false,
            ),
            DeviceNodeState::RemovePending => (
                DeviceNodeState::RemovePending,
                DeviceNodeState::Removing,
                true,
            ),
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: The registry serializes \
                          transitions and finish_publish runs once for a node it created in \
                          Publishing; removal moves Publishing only to RemovePending"
            )]
            state => panic!("cannot finish publishing a node in state {state:?}"),
        };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The device registry serializes state \
                      transitions, so the previously read state cannot change before \
                      compare_exchange"
        )]
        self.state
            .compare_exchange(
                current as u8,
                next as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .expect("device-node state transitions are serialized by the registry");
        should_remove
    }

    fn finish_removing(&self) {
        self.state
            .store(DeviceNodeState::Removed as u8, Ordering::Release);
    }
}

/// Stable device-node identity assigned by the device core.
#[derive(Debug, Clone)]
pub struct DeviceNode {
    core_id: DeviceId,
    kind: DeviceNodeKind,
    index: u32,
    device_number: DeviceNumber,
    devname: String,
    name: String,
    driver_name: String,
    location: DeviceLocation,
    input: Option<InputDeviceNodeMetadata>,
    lifecycle: Arc<DeviceNodeLifecycle>,
}

impl PartialEq for DeviceNode {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.lifecycle, &other.lifecycle)
    }
}

impl Eq for DeviceNode {}

impl DeviceNode {
    /// Driver-core identity for the backing device.
    pub const fn core_id(&self) -> DeviceId {
        self.core_id
    }

    /// Projected device-node class.
    pub const fn kind(&self) -> DeviceNodeKind {
        self.kind
    }

    /// Class-local Linux device index.
    pub const fn index(&self) -> u32 {
        self.index
    }

    /// Linux character-device number.
    pub const fn device_number(&self) -> DeviceNumber {
        self.device_number
    }

    /// Path below `/dev`.
    pub fn devname(&self) -> &str {
        &self.devname
    }

    /// Runtime device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Driver bound to the backing device.
    pub fn driver_name(&self) -> &str {
        &self.driver_name
    }

    /// Bus location of the backing device.
    pub const fn location(&self) -> DeviceLocation {
        self.location
    }

    /// Input-specific metadata, when this is an input device.
    pub fn input_metadata(&self) -> Option<&InputDeviceNodeMetadata> {
        self.input.as_ref()
    }

    /// Returns whether this exact node publication still accepts operations.
    ///
    /// Removal changes this state before notifying projection consumers. A
    /// projection should recheck it while holding its own access guard, so the
    /// remove callback can wait for all in-flight operations before publishing
    /// the remove event and invalidating filesystem entries.
    pub fn is_available(&self) -> bool {
        self.lifecycle.is_available()
    }

    fn finish_removing(&self) {
        self.lifecycle.finish_removing();
    }
}

/// Device-node lifecycle notification for projection consumers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNodeEvent {
    /// A fully assigned device node is available for projection.
    Added(DeviceNode),
    /// A device node has stopped accepting operations, but remains projected.
    Removing(DeviceNode),
    /// A device node may now be removed from projections.
    Removed(DeviceNode),
}

/// Callback invoked outside the device-node registry lock.
pub type DeviceNodeCallback = Arc<dyn Fn(DeviceNodeEvent) + Send + Sync>;

/// Ordered device-node lifecycle event delivered to the host event publisher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNodeLifecycleEvent {
    /// Projection subscribers have made the new node visible.
    Added(DeviceNode),
    /// Projection subscribers have quiesced node operations before deletion.
    Removed(DeviceNode),
}

/// Host callback that publishes externally visible device lifecycle events.
pub type DeviceNodeLifecycleCallback = Arc<dyn Fn(DeviceNodeLifecycleEvent) + Send + Sync>;

struct DeviceNodeRegistry {
    nodes: BTreeMap<DeviceId, DeviceNode>,
    input_indices: BTreeSet<u32>,
    drm_indices: BTreeSet<u32>,
    subscribers: Vec<DeviceNodeCallback>,
    lifecycle_publisher: Option<DeviceNodeLifecycleCallback>,
}

impl DeviceNodeRegistry {
    const fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            input_indices: BTreeSet::new(),
            drm_indices: BTreeSet::new(),
            subscribers: Vec::new(),
            lifecycle_publisher: None,
        }
    }

    fn indices(&mut self, kind: DeviceNodeKind) -> &mut BTreeSet<u32> {
        match kind {
            DeviceNodeKind::Input => &mut self.input_indices,
            DeviceNodeKind::Drm => &mut self.drm_indices,
        }
    }

    fn allocate_index(&mut self, kind: DeviceNodeKind) -> DriverResult<u32> {
        let used = self.indices(kind);
        let index = (0..kind.index_count())
            .find(|index| !used.contains(index))
            .ok_or(DriverError::ResourceBusy)?;
        used.insert(index);
        Ok(index)
    }

    fn free_index(&mut self, kind: DeviceNodeKind, index: u32) {
        self.indices(kind).remove(&index);
    }

    fn register(
        &mut self,
        core_id: DeviceId,
        class: DeviceNodeClass,
        name: String,
        driver_name: String,
        location: DeviceLocation,
    ) -> DriverResult<(
        DeviceNode,
        Vec<DeviceNodeCallback>,
        Option<DeviceNodeLifecycleCallback>,
        bool,
    )> {
        if let Some(node) = self.nodes.get(&core_id) {
            return if node.is_available() {
                Ok((node.clone(), Vec::new(), None, false))
            } else {
                Err(DriverError::ResourceBusy)
            };
        }
        let kind = class.kind();
        let index = self.allocate_index(kind)?;
        let node = DeviceNode {
            core_id,
            kind,
            index,
            device_number: kind.device_number(index),
            devname: kind.devname(index),
            name,
            driver_name,
            location,
            input: match class {
                DeviceNodeClass::Input(metadata) => Some(metadata),
                DeviceNodeClass::Drm => None,
            },
            lifecycle: Arc::new(DeviceNodeLifecycle::new()),
        };
        self.nodes.insert(core_id, node.clone());
        Ok((
            node,
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
            true,
        ))
    }

    fn finish_publish(
        &mut self,
        node: &DeviceNode,
    ) -> Option<(
        DeviceNode,
        Vec<DeviceNodeCallback>,
        Option<DeviceNodeLifecycleCallback>,
    )> {
        if self.nodes.get(&node.core_id()) != Some(node) {
            return None;
        }
        node.lifecycle.finish_publishing().then(|| {
            (
                node.clone(),
                self.subscribers.clone(),
                self.lifecycle_publisher.clone(),
            )
        })
    }

    fn begin_remove(
        &mut self,
        core_id: DeviceId,
    ) -> Option<(
        DeviceNode,
        Vec<DeviceNodeCallback>,
        Option<DeviceNodeLifecycleCallback>,
    )> {
        let node = self.nodes.get(&core_id)?.clone();
        if !node.lifecycle.request_removal() {
            return None;
        }
        Some((
            node,
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
        ))
    }

    fn snapshot(&self) -> Vec<DeviceNode> {
        self.nodes
            .values()
            .filter(|node| node.is_available())
            .cloned()
            .collect()
    }

    fn finish_remove(&mut self, node: &DeviceNode) {
        if self.nodes.get(&node.core_id()) == Some(node) {
            node.finish_removing();
            self.nodes.remove(&node.core_id());
            self.free_index(node.kind(), node.index());
        }
    }

    fn install_lifecycle_publisher(
        &mut self,
        callback: DeviceNodeLifecycleCallback,
    ) -> DriverResult<()> {
        if self.lifecycle_publisher.is_some() {
            return Err(DriverError::AlreadyExists);
        }
        if !self.nodes.is_empty() {
            return Err(DriverError::BadState);
        }
        self.lifecycle_publisher = Some(callback);
        Ok(())
    }
}

static DEVICE_NODES: LazyInit<SpinNoPreempt<DeviceNodeRegistry>> = LazyInit::new();

fn device_node_registry() -> SpinNoPreemptGuard<'static, DeviceNodeRegistry> {
    DEVICE_NODES.call_once(|| SpinNoPreempt::new(DeviceNodeRegistry::new()));
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed successfully and \
                  permanently published the device-node registry"
    )]
    DEVICE_NODES
        .get()
        .expect("device-node registry call_once succeeded")
        .lock()
}

fn dispatch_node_added(
    node: &DeviceNode,
    subscribers: Vec<DeviceNodeCallback>,
    publisher: Option<DeviceNodeLifecycleCallback>,
) {
    for subscriber in subscribers {
        subscriber(DeviceNodeEvent::Added(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DeviceNodeLifecycleEvent::Added(node.clone()));
    }
}

fn dispatch_node_removal(
    node: &DeviceNode,
    subscribers: Vec<DeviceNodeCallback>,
    publisher: Option<DeviceNodeLifecycleCallback>,
) {
    for subscriber in &subscribers {
        subscriber(DeviceNodeEvent::Removing(node.clone()));
    }
    if let Some(publisher) = publisher {
        publisher(DeviceNodeLifecycleEvent::Removed(node.clone()));
    }
    for subscriber in subscribers {
        subscriber(DeviceNodeEvent::Removed(node.clone()));
    }
}

/// Assign and publish a Linux-compatible node identity for a runtime device.
///
/// Re-registering the same available device id returns its existing
/// assignment. Node callbacks run synchronously after the registry lock is
/// released.
///
/// # Errors
///
/// Returns [`DriverError::ResourceBusy`] when the class-specific index range
/// is exhausted or the same device id is still completing removal.
pub fn register_device_node(
    core_id: DeviceId,
    class: DeviceNodeClass,
    name: String,
    driver_name: String,
    location: DeviceLocation,
) -> DriverResult<DeviceNode> {
    let (node, subscribers, publisher, is_new) =
        device_node_registry().register(core_id, class, name, driver_name, location)?;
    if !is_new {
        return Ok(node);
    }
    dispatch_node_added(&node, subscribers, publisher);
    let pending_removal = device_node_registry().finish_publish(&node);
    if let Some((removed, subscribers, publisher)) = pending_removal {
        dispatch_node_removal(&removed, subscribers, publisher);
        device_node_registry().finish_remove(&removed);
    }
    Ok(node)
}

/// Remove a device-node assignment and notify projection consumers.
///
/// The node first becomes unavailable, then projection consumers quiesce
/// operations, the host publisher emits remove, and projection consumers
/// delete their entries. The class index and device id remain reserved until
/// that sequence completes. If removal races the initial add callbacks, this
/// call records a pending removal; the thread publishing add completes the
/// ordered removal sequence.
pub fn unregister_device_node(core_id: DeviceId) {
    let (node, subscribers, publisher) = {
        let mut registry = device_node_registry();
        let Some(removal) = registry.begin_remove(core_id) else {
            return;
        };
        removal
    };
    dispatch_node_removal(&node, subscribers, publisher);
    device_node_registry().finish_remove(&node);
}

/// Return a snapshot of all current device-node assignments.
pub fn device_nodes_snapshot() -> Vec<DeviceNode> {
    device_node_registry().snapshot()
}

/// Subscribe to device-node changes and atomically return the current snapshot.
pub fn subscribe_device_nodes(callback: DeviceNodeCallback) -> Vec<DeviceNode> {
    let mut registry = device_node_registry();
    registry.subscribers.push(callback);
    registry.snapshot()
}

/// Installs the single host publisher for ordered device-node lifecycle events.
///
/// The publisher receives add only after projection subscribers have made the
/// node visible. During removal, projections first receive
/// [`DeviceNodeEvent::Removing`] so they can quiesce operations; the publisher
/// then receives remove while the paths still exist, and projections finally
/// receive [`DeviceNodeEvent::Removed`]. Installation must happen before the
/// first node is registered.
///
/// # Errors
///
/// Returns [`DriverError::AlreadyExists`] if a publisher is already installed,
/// or [`DriverError::BadState`] if nodes were registered before installation.
pub fn install_device_node_lifecycle_publisher(
    callback: DeviceNodeLifecycleCallback,
) -> DriverResult<()> {
    device_node_registry().install_lifecycle_publisher(callback)
}

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn node_kind_assigns_linux_name_and_number() {
        let input = DeviceNodeKind::Input;
        assert_eq!(input.devname(3), "input/event3");
        assert_eq!(input.device_number(3).major(), INPUT_MAJOR);
        assert_eq!(input.device_number(3).minor(), INPUT_EVENT_MINOR_BASE + 3);

        let drm = DeviceNodeKind::Drm;
        assert_eq!(drm.devname(2), "dri/card2");
        assert_eq!(drm.device_number(2).major(), DRM_MAJOR);
        assert_eq!(drm.device_number(2).minor(), 2);
    }

    #[def_test]
    fn class_index_is_reused_only_after_release() {
        let mut registry = DeviceNodeRegistry::new();
        assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(0));
        assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(1));
        registry.free_index(DeviceNodeKind::Input, 0);
        assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(0));
    }

    #[def_test]
    fn removal_keeps_the_old_index_reserved_until_callbacks_finish() {
        let mut registry = DeviceNodeRegistry::new();
        let (original, ..) = registry
            .register(
                DeviceId::new(0),
                DeviceNodeClass::Drm,
                String::from("original"),
                String::from("test"),
                DeviceLocation::PlatformStatic { id: 0 },
            )
            .expect("original node must fit");
        assert!(registry.finish_publish(&original).is_none());
        assert_eq!(original.index(), 0);

        let (removed, ..) = registry
            .begin_remove(original.core_id())
            .expect("the original node must be removable");
        assert!(!removed.is_available());
        let duplicate = registry.register(
            original.core_id(),
            DeviceNodeClass::Drm,
            String::from("duplicate"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
        );
        assert_eq!(duplicate.err(), Some(DriverError::ResourceBusy));
        let (replacement, ..) = registry
            .register(
                DeviceId::new(1),
                DeviceNodeClass::Drm,
                String::from("replacement"),
                String::from("test"),
                DeviceLocation::PlatformStatic { id: 1 },
            )
            .expect("replacement node must fit while the old index is reserved");
        assert_eq!(replacement.index(), 1);

        registry.finish_remove(&removed);
        let (later, ..) = registry
            .register(
                DeviceId::new(2),
                DeviceNodeClass::Drm,
                String::from("later"),
                String::from("test"),
                DeviceLocation::PlatformStatic { id: 2 },
            )
            .expect("the released index must be reusable");
        assert_eq!(later.index(), 0);
    }

    #[def_test]
    fn removal_requested_during_add_preserves_lifecycle_order() {
        let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
        let projection_observed = observed.clone();
        let publisher_observed = observed.clone();
        let mut registry = DeviceNodeRegistry::new();
        registry.subscribers.push(Arc::new(move |event| {
            projection_observed.lock().push(match event {
                DeviceNodeEvent::Added(_) => "projection-add",
                DeviceNodeEvent::Removing(_) => "projection-removing",
                DeviceNodeEvent::Removed(_) => "projection-removed",
            });
        }));
        registry
            .install_lifecycle_publisher(Arc::new(move |event| {
                publisher_observed.lock().push(match event {
                    DeviceNodeLifecycleEvent::Added(_) => "publisher-add",
                    DeviceNodeLifecycleEvent::Removed(_) => "publisher-remove",
                });
            }))
            .unwrap();

        let (node, subscribers, publisher, is_new) = registry
            .register(
                DeviceId::new(0),
                DeviceNodeClass::Drm,
                String::from("device"),
                String::from("test"),
                DeviceLocation::PlatformStatic { id: 0 },
            )
            .unwrap();
        assert!(is_new);
        assert!(registry.begin_remove(node.core_id()).is_none());
        assert!(!node.is_available());
        dispatch_node_added(&node, subscribers, publisher);
        let (node, subscribers, publisher) = registry
            .finish_publish(&node)
            .expect("pending removal must follow add publication");
        dispatch_node_removal(&node, subscribers, publisher);
        registry.finish_remove(&node);

        let observed = observed.lock();
        assert_eq!(
            observed.as_slice(),
            [
                "projection-add",
                "publisher-add",
                "projection-removing",
                "publisher-remove",
                "projection-removed",
            ]
        );
    }
}
