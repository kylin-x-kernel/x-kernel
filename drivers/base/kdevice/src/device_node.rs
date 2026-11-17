// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible device-node identities shared by filesystem projections.

use alloc::{
    boxed::Box,
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

use crate::{BindingRef, DeviceId, DeviceLocation, DeviceNumber, PciIdentity};

const INPUT_MAJOR: u32 = 13;
const INPUT_EVENT_MINOR_BASE: u32 = 64;
const INPUT_EVENT_COUNT: u32 = 32;
const DRM_MAJOR: u32 = 226;
const DRM_PRIMARY_COUNT: u32 = 64;

/// First Linux render-node minor; render nodes are numbered `base + card index`.
///
/// This is the only place that encodes the rule: devfs, sysfs and the uevent
/// publisher all derive the render minor from [`DeviceNode`] helpers instead of
/// repeating the offset.
pub const DRM_RENDER_MINOR_BASE: u32 = 128;

/// Device-node class projected by devtmpfs and sysfs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// The connector description of a DRM card, used by sysfs projection.
///
/// Linux publishes one `class/drm/cardN-<connector>` entry per connector
/// (`card0-Virtual-1`), and its attributes are the connector's state. This is
/// the snapshot a DRM class device supplies when it is published.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrmDeviceNodeMetadata {
    name: String,
    status: String,
    dpms: String,
    modes: String,
    edid: Vec<u8>,
}

impl DrmDeviceNodeMetadata {
    /// Creates a connector snapshot.
    ///
    /// `status`, `dpms` and `modes` are already in the form the sysfs
    /// attributes take, since their content is the producer's decision.
    pub fn new(name: String, status: String, dpms: String, modes: String, edid: Vec<u8>) -> Self {
        Self {
            name,
            status,
            dpms,
            modes,
            edid,
        }
    }

    /// Connector name, as it appears in the class entry (`Virtual-1`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The `status` attribute's text.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The `dpms` attribute's text.
    pub fn dpms(&self) -> &str {
        &self.dpms
    }

    /// The `modes` attribute's text.
    pub fn modes(&self) -> &str {
        &self.modes
    }

    /// The `edid` attribute's bytes.
    pub fn edid(&self) -> &[u8] {
        &self.edid
    }
}

/// Input identity and capability data used by sysfs projection consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceNodeClass {
    /// Input device with identity and capability metadata.
    Input(InputDeviceNodeMetadata),
    /// DRM primary device with its connector description.
    Drm(DrmDeviceNodeMetadata),
}

impl DeviceNodeClass {
    const fn kind(&self) -> DeviceNodeKind {
        match self {
            Self::Input(_) => DeviceNodeKind::Input,
            Self::Drm(_) => DeviceNodeKind::Drm,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug)]
pub struct DeviceNode {
    core_id: DeviceId,
    kind: DeviceNodeKind,
    index: u32,
    device_number: DeviceNumber,
    devname: String,
    name: String,
    driver_name: String,
    location: DeviceLocation,
    pci: Option<PciIdentity>,
    input: Option<InputDeviceNodeMetadata>,
    drm: Option<DrmDeviceNodeMetadata>,
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

    /// PCI identity of the parent device, when it lives on a PCI bus.
    ///
    /// sysfs publishes the PCI attributes (`vendor`, `device`,
    /// `subsystem_vendor`, `subsystem_device`, `class`) from this snapshot;
    /// libdrm identifies a DRM device through them.
    pub const fn pci_identity(&self) -> Option<PciIdentity> {
        self.pci
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

    /// Linux character-device number of the render node belonging to this node.
    ///
    /// Returns `None` for device classes that do not expose one. A DRM card
    /// always does, whether or not the bound driver advertises a render backend:
    /// Linux registers the minor unconditionally and decides per ioctl what the
    /// node accepts.
    pub const fn render_device_number(&self) -> Option<DeviceNumber> {
        match self.kind {
            DeviceNodeKind::Drm => Some(DeviceNumber::new(
                DRM_MAJOR,
                DRM_RENDER_MINOR_BASE + self.index,
            )),
            DeviceNodeKind::Input => None,
        }
    }

    /// Path below `/dev` for the render node, e.g. `dri/renderD128`.
    pub fn render_devname(&self) -> Option<String> {
        (self.kind == DeviceNodeKind::Drm)
            .then(|| format!("dri/renderD{}", DRM_RENDER_MINOR_BASE + self.index))
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

    /// DRM-specific metadata, when this is a DRM card.
    pub fn drm_metadata(&self) -> Option<&DrmDeviceNodeMetadata> {
        self.drm.as_ref()
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceNodeLifecycleEvent {
    /// Projection subscribers have made the new node visible.
    Added(DeviceNode),
    /// Projection subscribers have quiesced node operations before deletion.
    Removed(DeviceNode),
}

/// Host callback that publishes externally visible device lifecycle events.
pub type DeviceNodeLifecycleCallback = Arc<dyn Fn(DeviceNodeLifecycleEvent) + Send + Sync>;

/// Cleanup callback deferred because a node was busy publishing or removing.
///
/// It runs exactly once, on the thread that finally retires the node instance,
/// outside the node registry lock. Class code uses it to retire the exact
/// binding backed by that node.
pub type DeferredNodeRetirement = Box<dyn FnOnce() + Send + 'static>;

/// Removal work dispatched after the node registry lock is released.
type NodeDispatch = (
    DeviceNode,
    Vec<DeviceNodeCallback>,
    Option<DeviceNodeLifecycleCallback>,
);

/// Outcome of a binding-owned node removal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeRemoval {
    /// The node was made unavailable and fully removed synchronously.
    Removed,
    /// The node was publishing/removing; `on_deferred` was registered and will
    /// run exactly once when that node instance is retired.
    Deferred,
    /// The node already has a deferred completion owner. The callback supplied
    /// by this call was not registered; it is dropped after the registry lock
    /// is released. The existing callback owns the retirement.
    DeferredExisting,
    /// No live node exists for the id.
    NotFound,
    /// The caller's binding does not own the current node.
    NotOwner,
}

struct DeviceNodeRegistry {
    nodes: BTreeMap<DeviceId, DeviceNode>,
    /// Binding that owns each live node. Nodes registered through the legacy
    /// ownerless API have no entry.
    bindings: BTreeMap<DeviceId, BindingRef>,
    /// Cleanup callbacks waiting for a busy node instance to finish.
    deferred_retirements: BTreeMap<DeviceId, DeferredNodeRetirement>,
    input_indices: BTreeSet<u32>,
    drm_indices: BTreeSet<u32>,
    subscribers: Vec<DeviceNodeCallback>,
    lifecycle_publisher: Option<DeviceNodeLifecycleCallback>,
}

impl DeviceNodeRegistry {
    const fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            bindings: BTreeMap::new(),
            deferred_retirements: BTreeMap::new(),
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

    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: internal registration entry point mirrors the public \
                  device-node facts (id, owner, class, name, driver, location, PCI identity); a \
                  builder would duplicate both public wrappers without reducing the validated \
                  field set"
    )]
    fn register(
        &mut self,
        core_id: DeviceId,
        owner: Option<&BindingRef>,
        class: DeviceNodeClass,
        name: String,
        driver_name: String,
        location: DeviceLocation,
        pci: Option<PciIdentity>,
    ) -> DriverResult<(
        DeviceNode,
        Vec<DeviceNodeCallback>,
        Option<DeviceNodeLifecycleCallback>,
        bool,
    )> {
        if let Some(node) = self.nodes.get(&core_id) {
            let same_owner = match owner {
                Some(owner) => self
                    .bindings
                    .get(&core_id)
                    .is_some_and(|current| current.same_binding(owner)),
                // The legacy ownerless API may only reuse an ownerless node.
                None => !self.bindings.contains_key(&core_id),
            };
            return if same_owner && node.is_available() {
                Ok((node.clone(), Vec::new(), None, false))
            } else {
                // A removed/terminal A must be evicted first; never let a new
                // binding silently take over its node instance.
                Err(DriverError::ResourceBusy)
            };
        }
        // A stale owner entry without a node cannot happen when every removal
        // path clears it; drop it rather than letting it block a fresh node.
        self.bindings.remove(&core_id);
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
            pci,
            input: match &class {
                DeviceNodeClass::Input(metadata) => Some(metadata.clone()),
                DeviceNodeClass::Drm(_) => None,
            },
            drm: match class {
                DeviceNodeClass::Drm(metadata) => Some(metadata),
                DeviceNodeClass::Input(_) => None,
            },
            lifecycle: Arc::new(DeviceNodeLifecycle::new()),
        };
        self.nodes.insert(core_id, node.clone());
        if let Some(owner) = owner {
            self.bindings.insert(core_id, owner.clone());
        }
        Ok((
            node,
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
            true,
        ))
    }

    /// Complete publication, or report that removal was requested while the
    /// node was publishing.
    ///
    /// On the removal path the node is left registered in `Removing` with its
    /// class index still reserved. The caller must dispatch the removal
    /// sequence outside the lock and only then call [`Self::finish_remove`]:
    /// freeing the node and its index before the `Removing` notification would
    /// let a newly discovered device take the same index while consumers still
    /// observe the old removal.
    fn finish_publish(&mut self, node: &DeviceNode) -> Option<NodeDispatch> {
        if self.nodes.get(&node.core_id()) != Some(node) {
            return None;
        }
        if !node.lifecycle.finish_publishing() {
            return None;
        }
        Some((
            node.clone(),
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
        ))
    }

    /// Start removal for a binding-owned node.
    ///
    /// `Ok(Some(...))` means the caller must dispatch the removal outside the
    /// lock; `Ok(None)` means the callback was registered on a busy node.
    fn begin_remove_for_binding(
        &mut self,
        core_id: DeviceId,
        owner: &BindingRef,
        on_deferred: &mut Option<DeferredNodeRetirement>,
    ) -> Result<Option<NodeDispatch>, NodeRemoval> {
        let Some(node) = self.nodes.get(&core_id) else {
            return Err(NodeRemoval::NotFound);
        };
        if !self
            .bindings
            .get(&core_id)
            .is_some_and(|current| current.same_binding(owner))
        {
            return Err(NodeRemoval::NotOwner);
        }
        if !node.lifecycle.request_removal() {
            if self.deferred_retirements.contains_key(&core_id) {
                return Err(NodeRemoval::DeferredExisting);
            }
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: the only caller passes Some and \
                          takes the callback only on this first-wins registration path"
            )]
            let callback = on_deferred
                .take()
                .expect("begin_remove_for_binding requires a deferred callback");
            self.deferred_retirements.insert(core_id, callback);
            return Ok(None);
        }
        Ok(Some((
            node.clone(),
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
        )))
    }

    /// Start removal for a legacy ownerless node.
    ///
    /// `Err(NodeRemoval::NotOwner)` means a binding owns the node and the
    /// legacy API must not touch it.
    fn begin_remove_ownerless(
        &mut self,
        core_id: DeviceId,
    ) -> Result<Option<NodeDispatch>, NodeRemoval> {
        if self.bindings.contains_key(&core_id) {
            return Err(NodeRemoval::NotOwner);
        }
        let Some(node) = self.nodes.get(&core_id) else {
            return Ok(None);
        };
        if !node.lifecycle.request_removal() {
            return Ok(None);
        }
        Ok(Some((
            node.clone(),
            self.subscribers.clone(),
            self.lifecycle_publisher.clone(),
        )))
    }

    fn snapshot(&self) -> Vec<DeviceNode> {
        self.nodes
            .values()
            .filter(|node| node.is_available())
            .cloned()
            .collect()
    }

    /// Retire the exact node instance and take its deferred cleanup callback.
    ///
    /// The node, its class index and its owner mapping are removed only when
    /// the expected owner still matches (or, for the ownerless API, only when
    /// no owner was ever recorded). The owner check precedes every deletion:
    /// a stale cleanup must never erase a newer owner's node, index or
    /// deferred retirement.
    fn finish_remove(
        &mut self,
        node: &DeviceNode,
        expected_owner: Option<&BindingRef>,
    ) -> Option<DeferredNodeRetirement> {
        let core_id = node.core_id();
        if self.nodes.get(&core_id) != Some(node) {
            return None;
        }
        let owner_matches = match expected_owner {
            Some(expected) => self
                .bindings
                .get(&core_id)
                .is_some_and(|current| current.same_binding(expected)),
            None => !self.bindings.contains_key(&core_id),
        };
        if !owner_matches {
            return None;
        }
        node.finish_removing();
        self.nodes.remove(&core_id);
        self.free_index(node.kind(), node.index());
        self.bindings.remove(&core_id);
        self.deferred_retirements.remove(&core_id)
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
    #[cfg(unittest)]
    crate::lifecycle::test_hooks::pause(
        crate::lifecycle::test_hooks::BindingPause::NodePublishing {
            device_id: node.core_id(),
        },
    );
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
    #[cfg(unittest)]
    crate::lifecycle::test_hooks::pause(crate::lifecycle::test_hooks::BindingPause::NodeRemoving {
        device_id: node.core_id(),
    });
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
    pci: Option<PciIdentity>,
) -> DriverResult<DeviceNode> {
    let (node, subscribers, publisher, is_new) =
        device_node_registry().register(core_id, None, class, name, driver_name, location, pci)?;
    if !is_new {
        return Ok(node);
    }
    dispatch_node_added(&node, subscribers, publisher);
    let pending_removal = device_node_registry().finish_publish(&node);
    if let Some((removed, subscribers, publisher)) = pending_removal {
        // The node stays registered in `Removing` with its index reserved
        // until the removal notification has run.
        dispatch_node_removal(&removed, subscribers, publisher);
        let deferred = device_node_registry().finish_remove(&removed, None);
        if let Some(deferred) = deferred {
            deferred();
        }
    }
    Ok(node)
}

/// Assign and publish a node identity owned by one binding instance.
///
/// A node already owned by the same binding and still available is returned
/// idempotently. A node owned by any other binding, or one still publishing or
/// removing, always yields [`DriverError::ResourceBusy`]: a new binding may
/// never take over an old binding's node instance.
pub fn register_device_node_for_binding(
    core_id: DeviceId,
    binding: &BindingRef,
    class: DeviceNodeClass,
    name: String,
    driver_name: String,
    location: DeviceLocation,
    pci: Option<PciIdentity>,
) -> DriverResult<DeviceNode> {
    let (node, subscribers, publisher, is_new) = device_node_registry().register(
        core_id,
        Some(binding),
        class,
        name,
        driver_name,
        location,
        pci,
    )?;
    if !is_new {
        return Ok(node);
    }
    dispatch_node_added(&node, subscribers, publisher);
    let pending_removal = device_node_registry().finish_publish(&node);
    if let Some((removed, subscribers, publisher)) = pending_removal {
        // The node stays registered in `Removing` with its index reserved
        // until the removal notification has run.
        dispatch_node_removal(&removed, subscribers, publisher);
        let deferred = device_node_registry().finish_remove(&removed, Some(binding));
        if let Some(deferred) = deferred {
            deferred();
        }
    }
    Ok(node)
}

/// Remove a legacy ownerless device-node assignment and notify consumers.
///
/// The node first becomes unavailable, then projection consumers quiesce
/// operations, the host publisher emits remove, and projection consumers
/// delete their entries. If removal races the initial add callbacks, this call
/// records a pending removal; the thread publishing add completes the ordered
/// removal sequence.
///
/// # Errors
///
/// Returns [`DriverError::BadState`] if the node is owned by a binding; the
/// legacy ID-only API must not remove another binding's node.
pub fn unregister_device_node(core_id: DeviceId) -> DriverResult<()> {
    let start = {
        let mut registry = device_node_registry();
        registry.begin_remove_ownerless(core_id)
    };
    let Some((node, subscribers, publisher)) = (match start {
        Ok(removal) => removal,
        Err(_) => return Err(DriverError::BadState),
    }) else {
        return Ok(());
    };
    dispatch_node_removal(&node, subscribers, publisher);
    let deferred = device_node_registry().finish_remove(&node, None);
    if let Some(deferred) = deferred {
        deferred();
    }
    Ok(())
}

/// Remove a node owned by `binding`, deferring completion if the node is busy.
///
/// The owner check, the state transition and deferred-callback registration all
/// happen in one registry critical section. On [`NodeRemoval::Deferred`] the
/// provided callback was registered and runs exactly once when that node
/// instance finishes; the caller must not also run it. On
/// [`NodeRemoval::DeferredExisting`] another request already owns completion;
/// the callback supplied by this call is not registered and is dropped after
/// the registry lock is released. Duplicate retirement requests are merged
/// first-wins, so callers must treat the callback as an idempotent completion
/// notification rather than an additional executor.
pub fn unregister_device_node_for_binding(
    core_id: DeviceId,
    binding: &BindingRef,
    on_deferred: DeferredNodeRetirement,
) -> NodeRemoval {
    // Keep the callback outside the registry guard. Any path that does not
    // register it returns it here, so captured resources are dropped after the
    // spinlock is released.
    let mut unused_callback = Some(on_deferred);
    let start = {
        let mut registry = device_node_registry();
        registry.begin_remove_for_binding(core_id, binding, &mut unused_callback)
    };

    let result = match start {
        Ok(Some((node, subscribers, publisher))) => {
            dispatch_node_removal(&node, subscribers, publisher);
            let deferred = device_node_registry().finish_remove(&node, Some(binding));
            if let Some(deferred) = deferred {
                deferred();
            }
            NodeRemoval::Removed
        }
        Ok(None) => NodeRemoval::Deferred,
        Err(NodeRemoval::DeferredExisting) => NodeRemoval::DeferredExisting,
        Err(NodeRemoval::NotFound) => NodeRemoval::NotFound,
        Err(_) => NodeRemoval::NotOwner,
    };
    drop(unused_callback.take());
    result
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

unittest::path_mod_test!(tests, "../ut-tests/device_node.rs");
