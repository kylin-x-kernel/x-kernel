// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux-compatible, read-mostly device discovery filesystem.
//!
//! This crate projects [`kdevice::DeviceNode`] registrations into dynamic
//! `/sys/devices`, `/sys/class`, `/sys/dev/char`, and `/sys/bus` trees. It also
//! exposes device attributes and accepts validated user-triggered kobject
//! uevents for device-management clients.
//! Mount the filesystem through the VFS using [`FILE_SYSTEM_TYPE`]; callers do
//! not construct its singleton superblock directly.
//!
//! # Usage
//!
//! From userspace after the filesystem type has been registered:
//!
//! ```text
//! mount -t sysfs sysfs /sys
//! ```
//!
//! The architecture and trust boundaries are described in
//! `fs/filesystems/sysfs/docs/design.md` and
//! `fs/filesystems/sysfs/docs/security.md`.

#![no_std]

extern crate alloc;

use alloc::{
    borrow::Cow,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use kalloc_types::KBox;
use kdevice::{
    DeviceLocation, DeviceNode, DeviceNodeEvent, DeviceNodeKind, subscribe_device_nodes,
};
use kdevice_uevent::{self, UeventAction, UeventScope};
use klazy::Once;
use ksync::{Mutex, RwLock};
use kvfs::{
    CommandFile, Dentry, FileSystemType, FsContext, FsContextOperations, Mount, NodePermission,
    NodeType, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFile, SimpleFileOperation, SimpleFs,
    SuperBlock, SuperBlockFlags, VfsError, VfsResult, get_tree_nodev,
};
use lazyinit::LazyInit;

const SYSFS_MAGIC: u32 = 0x6265_6572;
const READ_ONLY_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o444);
const UEVENT_PERMISSION: NodePermission = NodePermission::from_bits_truncate(0o644);

static SYSFS: Once<(Arc<SuperBlock>, Arc<Mount>)> = Once::new();
static DEVICE_PROJECTION_INITIALIZATION: Once<()> = Once::new();
static DEVICE_PROJECTION: LazyInit<Mutex<DeviceProjection>> = LazyInit::new();
static DEVICE_PROJECTION_EVENTS: LazyInit<Mutex<()>> = LazyInit::new();

#[derive(Debug)]
struct ProjectedDevice {
    node: DeviceNode,
    access: RwLock<()>,
}

impl ProjectedDevice {
    fn new(node: DeviceNode) -> Self {
        Self {
            node,
            access: RwLock::new(()),
        }
    }

    fn with_available<R>(&self, operation: impl FnOnce(&DeviceNode) -> R) -> VfsResult<R> {
        if !self.node.is_available() {
            return Err(VfsError::NoSuchDevice);
        }
        let _access = self.access.read();
        if !self.node.is_available() {
            return Err(VfsError::NoSuchDevice);
        }
        Ok(operation(&self.node))
    }
}

struct DirectoryEntry {
    inode: Once<Arc<kvfs::VfsInode>>,
}

impl DirectoryEntry {
    const fn new() -> Self {
        Self { inode: Once::new() }
    }
}

#[derive(Clone)]
enum TreeEntry {
    Directory(Arc<DirectoryEntry>),
    CharacterAttribute(kvfs::CharacterDeviceRegistration, Vec<u8>),
    Attribute(Arc<ProjectedDevice>, Vec<u8>),
    Symlink(String),
    Uevent(Arc<ProjectedDevice>, UeventScope),
    RawUevent {
        path: String,
        subsystem: String,
        properties: Vec<(String, String)>,
        contents: Vec<u8>,
    },
}

impl TreeEntry {
    fn has_same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::CharacterAttribute(left, left_data),
                Self::CharacterAttribute(right, right_data),
            ) => left.same_registration(right) && left_data == right_data,
            (Self::Directory(left), Self::Directory(right)) => Arc::ptr_eq(left, right),
            (Self::Attribute(left, left_data), Self::Attribute(right, right_data)) => {
                Arc::ptr_eq(left, right) && left_data == right_data
            }
            (Self::Symlink(left), Self::Symlink(right)) => left == right,
            (Self::Uevent(left, left_scope), Self::Uevent(right, right_scope)) => {
                Arc::ptr_eq(left, right) && left_scope == right_scope
            }
            (
                Self::RawUevent {
                    path: left_path,
                    subsystem: left_subsystem,
                    properties: left_properties,
                    contents: left_contents,
                },
                Self::RawUevent {
                    path: right_path,
                    subsystem: right_subsystem,
                    properties: right_properties,
                    contents: right_contents,
                },
            ) => {
                left_path == right_path
                    && left_subsystem == right_subsystem
                    && left_properties == right_properties
                    && left_contents == right_contents
            }
            _ => false,
        }
    }
}

struct DeviceProjection {
    nodes: BTreeMap<kdevice::DeviceId, Arc<ProjectedDevice>>,
    characters: Vec<kvfs::CharacterDeviceRegistration>,
    tree: BTreeMap<String, TreeEntry>,
}

impl DeviceProjection {
    fn new() -> Self {
        let nodes = BTreeMap::new();
        let characters = Vec::new();
        let tree = build_tree(&nodes, &characters);
        Self {
            nodes,
            characters,
            tree,
        }
    }

    fn rebuild_tree(&mut self) -> Vec<String> {
        let mut tree = build_tree(&self.nodes, &self.characters);
        for (path, entry) in &mut tree {
            if let TreeEntry::Directory(directory) = entry
                && let Some(TreeEntry::Directory(previous)) = self.tree.get(path)
            {
                *directory = previous.clone();
            }
        }
        let changed = changed_paths(&self.tree, &tree);
        self.tree = tree;
        changed
    }
}

/// Strips the one trailing newline or NUL `kobject_action_type` ignores.
fn strip_action_terminator(data: &[u8]) -> &[u8] {
    match data {
        [rest @ .., b'\n' | 0] => rest,
        _ => data,
    }
}

/// Parses a `uevent` write the way `kobject_action_type` does.
///
/// One trailing newline or NUL is dropped, then the action is the first token
/// up to a single space: `udevadm trigger` writes `add <uuid>` and userspace
/// may append further `KEY=VALUE` words. Any other whitespace stays inside the
/// token, so `add\t<uuid>`, an embedded newline or a leading tab does not name
/// an action and the write is refused, matching the reference parser.
fn parse_uevent_action(data: &[u8]) -> VfsResult<UeventAction> {
    let text = strip_action_terminator(data);
    let text = core::str::from_utf8(text).map_err(|_| VfsError::InvalidInput)?;
    let action = text.split(' ').next().ok_or(VfsError::InvalidInput)?;
    UeventAction::try_from(action).map_err(|_| VfsError::InvalidInput)
}

/// The device projection tree shared by every lookup.
fn device_projection() -> &'static Mutex<DeviceProjection> {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Device projection initialization installs \
                  both permanent locks before callbacks or lookups are exposed"
    )]
    DEVICE_PROJECTION
        .get()
        .expect("sysfs device projection must initialize before lookup")
}

fn device_projection_events() -> &'static Mutex<()> {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Device projection initialization installs \
                  both permanent locks before callbacks or lookups are exposed"
    )]
    DEVICE_PROJECTION_EVENTS
        .get()
        .expect("sysfs device projection event serialization must initialize before callbacks")
}

/// Initializes the device-node projection independently of mounting sysfs.
///
/// Boot calls this before driver discovery so every lifecycle add event first
/// creates its sysfs projection. Mount-time initialization remains as an
/// idempotent fallback for nonstandard callers.
pub fn init_device_projection() {
    DEVICE_PROJECTION_INITIALIZATION.call_once(|| {
        DEVICE_PROJECTION.init_once(Mutex::new(DeviceProjection::new()));
        DEVICE_PROJECTION_EVENTS.init_once(Mutex::new(()));

        let mut projection = device_projection().lock();
        // kdevice installs the subscriber and captures the snapshot under its
        // registry lock, but invokes callbacks only after releasing that lock.
        // Holding this mutex therefore serializes callbacks without creating a
        // lock cycle and prevents a stale snapshot from overtaking removal.
        let nodes = subscribe_device_nodes(Arc::new(handle_device_event));
        for node in nodes {
            projection
                .nodes
                .entry(node.core_id())
                .or_insert_with(|| Arc::new(ProjectedDevice::new(node)));
        }
        projection.characters = kvfs::subscribe_character_devices(Arc::new(handle_character_event));
        projection.rebuild_tree();
        drop(projection);
    });
}

fn handle_character_event() {
    let _event = device_projection_events().lock();
    let mut projection = device_projection().lock();
    projection.characters = kvfs::registered_character_devices();
    let changed = projection.rebuild_tree();
    drop(projection);
    invalidate_changed_paths(&changed);
}

fn handle_device_event(event: DeviceNodeEvent) {
    let _event = device_projection_events().lock();
    match event {
        DeviceNodeEvent::Added(node) => {
            let mut projection = device_projection().lock();
            let is_same_publication = projection
                .nodes
                .get(&node.core_id())
                .is_some_and(|projected| projected.node == node);
            if !is_same_publication {
                projection
                    .nodes
                    .insert(node.core_id(), Arc::new(ProjectedDevice::new(node)));
            }
            let changed = projection.rebuild_tree();
            drop(projection);
            invalidate_changed_paths(&changed);
        }
        DeviceNodeEvent::Removing(node) => {
            let projected = device_projection()
                .lock()
                .nodes
                .get(&node.core_id())
                .filter(|projected| projected.node == node)
                .cloned();
            if let Some(projected) = projected {
                // `kdevice` marks the node unavailable before this callback.
                // The exclusive guard waits for reads or uevent writes that
                // already passed their availability check.
                let _access = projected.access.write();
            }
        }
        DeviceNodeEvent::Removed(node) => {
            let mut projection = device_projection().lock();
            // A device with the same core ID may have been republished while
            // removal callbacks ran. Do not let the stale event delete its
            // replacement projection.
            if projection
                .nodes
                .get(&node.core_id())
                .is_some_and(|projected| projected.node == node)
            {
                projection.nodes.remove(&node.core_id());
                let changed = projection.rebuild_tree();
                drop(projection);
                invalidate_changed_paths(&changed);
            }
        }
    }
}

fn get_tree(
    context: &mut FsContext<'_>,
    _lookup_root: &kvfs::Path,
    _lookup_pwd: &kvfs::Path,
) -> VfsResult<Arc<SuperBlock>> {
    get_tree_nodev(context, |file_system_type, flags| {
        new_sysfs_with_type(file_system_type, flags)
    })
}

static FS_CONTEXT_OPERATIONS: FsContextOperations = FsContextOperations::new(get_tree);

fn init_fs_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&FS_CONTEXT_OPERATIONS);
    Ok(())
}

/// Registered sysfs filesystem type.
pub static FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("sysfs", init_fs_context);

#[macros::register_init]
fn init_sysfs() {
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Each statically named filesystem is \
                  registered once by its unique linker initialization entry"
    )]
    kvfs::register_filesystem(&FILE_SYSTEM_TYPE).expect("sysfs filesystem type must register once");
}

fn new_sysfs_with_type(
    file_system_type: &'static FileSystemType,
    superblock_flags: SuperBlockFlags,
) -> VfsResult<Arc<SuperBlock>> {
    init_device_projection();
    let (super_block, _internal_mount) = SYSFS.try_call_once(|| {
        let super_block = SimpleFs::new_with_allocation_flags(
            file_system_type,
            SYSFS_MAGIC,
            superblock_flags,
            kalloc_types::AllocFlags::NOWAIT,
            |fs| {
                SimpleDir::new_maker(
                    fs.clone(),
                    Arc::new(SysfsDirectory {
                        fs,
                        path: String::new(),
                    }),
                )
            },
        )?;
        let internal_mount = Mount::new_root(&super_block);
        Ok::<_, kvfs::VfsError>((super_block, internal_mount))
    })?;
    Ok(super_block.clone())
}

fn add_directory(tree: &mut BTreeMap<String, TreeEntry>, path: &str) {
    let mut current = String::new();
    for component in path.split('/').filter(|component| !component.is_empty()) {
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(component);
        tree.entry(current.clone())
            .or_insert_with(|| TreeEntry::Directory(Arc::new(DirectoryEntry::new())));
    }
}

fn add_entry(tree: &mut BTreeMap<String, TreeEntry>, path: String, entry: TreeEntry) {
    if let Some((parent, _)) = path.rsplit_once('/') {
        add_directory(tree, parent);
    }
    tree.insert(path, entry);
}

fn changed_paths(
    previous: &BTreeMap<String, TreeEntry>,
    current: &BTreeMap<String, TreeEntry>,
) -> Vec<String> {
    let mut changed: Vec<String> = previous
        .iter()
        .filter(|(path, entry)| {
            current
                .get(*path)
                .is_none_or(|current| !entry.has_same_identity(current))
        })
        .map(|(path, _)| path.clone())
        .collect();
    changed.extend(
        current
            .keys()
            .filter(|path| !previous.contains_key(*path))
            .cloned(),
    );
    changed.sort_by(|left, right| {
        left.matches('/')
            .count()
            .cmp(&right.matches('/').count())
            .then(left.cmp(right))
    });

    let mut topmost = Vec::<String>::new();
    for path in changed {
        let has_changed_ancestor = topmost.iter().any(|ancestor| {
            path.strip_prefix(ancestor)
                .is_some_and(|remainder| remainder.starts_with('/'))
        });
        if !has_changed_ancestor {
            topmost.push(path);
        }
    }
    topmost
}

fn invalidate_changed_paths(paths: &[String]) {
    let Some((super_block, _internal_mount)) = SYSFS.get() else {
        return;
    };
    let root = super_block.root_dir();
    for path in paths {
        let mut parent = root.clone();
        let mut components = path.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                // Invalidation is idempotent; an absent cached entry is already the goal.
                let _ = parent.invalidate_cached_child(component);
                break;
            }
            let Some(child) = parent.lookup_cache(component) else {
                break;
            };
            if !child.is_really_positive() || !child.can_lookup() {
                // Invalidation is idempotent; an absent cached entry is already the goal.
                let _ = parent.invalidate_cached_child(component);
                break;
            }
            parent = child;
        }
    }
}

fn relative_symlink(source: &str, target: &str) -> String {
    let source_parent = source.rsplit_once('/').map_or("", |(parent, _)| parent);
    let source_components: Vec<&str> = source_parent
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let target_components: Vec<&str> = target
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let common_components = source_components
        .iter()
        .zip(&target_components)
        .take_while(|(source, target)| source == target)
        .count();

    let mut result = String::new();
    for _ in common_components..source_components.len() {
        result.push_str("../");
    }
    let target_remainder = target_components[common_components..].join("/");
    if target_remainder.is_empty() {
        result.pop();
    } else {
        result.push_str(&target_remainder);
    }
    if result.is_empty() {
        ".".to_string()
    } else {
        result
    }
}

fn bus_name(location: DeviceLocation) -> &'static str {
    match location {
        DeviceLocation::Pci { .. } | DeviceLocation::Bridge { .. } => "pci",
        _ => "platform",
    }
}

fn append_line(value: &str) -> Vec<u8> {
    let mut bytes = value.as_bytes().to_vec();
    bytes.push(b'\n');
    bytes
}

fn render_uevent(properties: &[(String, String)]) -> Vec<u8> {
    let mut contents = String::new();
    for (key, value) in properties {
        contents.push_str(key);
        contents.push('=');
        contents.push_str(value);
        contents.push('\n');
    }
    contents.into_bytes()
}

fn bitmap_text(bytes: &[u8]) -> Vec<u8> {
    append_line(&kdevice_uevent::bitmap_value(bytes))
}

fn add_common_device_entries(
    tree: &mut BTreeMap<String, TreeEntry>,
    projected: &Arc<ProjectedDevice>,
    leaf: &str,
) {
    let node = &projected.node;
    let number = node.device_number();
    add_entry(
        tree,
        format!("{leaf}/dev"),
        TreeEntry::Attribute(
            projected.clone(),
            append_line(&format!("{}:{}", number.major(), number.minor())),
        ),
    );
    add_entry(
        tree,
        format!("{leaf}/uevent"),
        TreeEntry::Uevent(projected.clone(), UeventScope::Device),
    );
    let subsystem_link = format!("{leaf}/subsystem");
    add_entry(
        tree,
        subsystem_link.clone(),
        TreeEntry::Symlink(relative_symlink(
            &subsystem_link,
            &format!("class/{}", kdevice_uevent::subsystem_name(node)),
        )),
    );
    let character_device_link = format!("dev/char/{}:{}", number.major(), number.minor());
    add_entry(
        tree,
        character_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&character_device_link, leaf)),
    );
}

fn add_input_entries(tree: &mut BTreeMap<String, TreeEntry>, projected: &Arc<ProjectedDevice>) {
    let node = &projected.node;
    let physical = kdevice_uevent::physical_device_path(node.location());
    let input = format!("devices/{physical}/input/input{}", node.index());
    let event = format!("{input}/event{}", node.index());
    add_directory(tree, &event);
    add_common_device_entries(tree, projected, &event);
    let event_device_link = format!("{event}/device");
    add_entry(
        tree,
        event_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&event_device_link, &input)),
    );
    let input_class_link = format!("class/input/input{}", node.index());
    add_entry(
        tree,
        input_class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&input_class_link, &input)),
    );
    let event_class_link = format!("class/input/event{}", node.index());
    add_entry(
        tree,
        event_class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&event_class_link, &event)),
    );
    let input_subsystem_link = format!("{input}/subsystem");
    add_entry(
        tree,
        input_subsystem_link.clone(),
        TreeEntry::Symlink(relative_symlink(&input_subsystem_link, "class/input")),
    );
    add_entry(
        tree,
        format!("{input}/uevent"),
        TreeEntry::Uevent(projected.clone(), UeventScope::InputParent),
    );

    let Some(metadata) = node.input_metadata() else {
        return;
    };
    add_entry(
        tree,
        format!("{input}/name"),
        TreeEntry::Attribute(projected.clone(), append_line(metadata.name())),
    );
    add_entry(
        tree,
        format!("{input}/phys"),
        TreeEntry::Attribute(projected.clone(), append_line(metadata.physical_location())),
    );
    add_entry(
        tree,
        format!("{input}/uniq"),
        TreeEntry::Attribute(projected.clone(), append_line(metadata.unique_id())),
    );
    add_entry(
        tree,
        format!("{input}/properties"),
        TreeEntry::Attribute(projected.clone(), append_line("0")),
    );
    let id = metadata.device_id();
    for (name, value) in [
        ("bustype", id.bus_type),
        ("vendor", id.vendor),
        ("product", id.product),
        ("version", id.version),
    ] {
        add_entry(
            tree,
            format!("{input}/id/{name}"),
            TreeEntry::Attribute(projected.clone(), append_line(&format!("{value:04x}"))),
        );
    }
    add_entry(
        tree,
        format!("{input}/capabilities/ev"),
        TreeEntry::Attribute(projected.clone(), bitmap_text(metadata.event_types())),
    );
    for (event_type, name) in [
        (1, "key"),
        (2, "rel"),
        (3, "abs"),
        (4, "msc"),
        (5, "sw"),
        (0x11, "led"),
        (0x12, "snd"),
        (0x15, "ff"),
    ] {
        let bits = metadata.event_codes(event_type).unwrap_or(&[0]);
        add_entry(
            tree,
            format!("{input}/capabilities/{name}"),
            TreeEntry::Attribute(projected.clone(), bitmap_text(bits)),
        );
    }
}

fn add_drm_entries(tree: &mut BTreeMap<String, TreeEntry>, projected: &Arc<ProjectedDevice>) {
    let node = &projected.node;
    let physical = kdevice_uevent::physical_device_path(node.location());
    let card = format!("devices/{physical}/drm/card{}", node.index());
    add_directory(tree, &card);
    add_common_device_entries(tree, projected, &card);
    let physical_device = format!("devices/{physical}");
    let card_device_link = format!("{card}/device");
    add_entry(
        tree,
        card_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&card_device_link, &physical_device)),
    );
    let class_link = format!("class/drm/card{}", node.index());
    add_entry(
        tree,
        class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&class_link, &card)),
    );
    // Linux publishes one class entry per connector, named after the card and
    // the connector (`card0-Virtual-1`), and its attributes are the
    // connector's state.
    if let Some(connector) = node.drm_metadata() {
        // The class entry links to the connector's *own* directory
        // (`.../drm/card0-Virtual-1`), which is where its attributes live;
        // linking to the card would hide every one of them.
        let directory = format!("{card}-{}", connector.name());
        let connector_leaf = format!("{class_link}-{}", connector.name());
        add_entry(
            tree,
            connector_leaf.clone(),
            TreeEntry::Symlink(relative_symlink(&connector_leaf, &directory)),
        );
        add_directory(tree, &directory);
        for (attribute, contents) in [
            ("status", String::from(connector.status())),
            ("dpms", String::from(connector.dpms())),
            ("modes", String::from(connector.modes())),
            ("enabled", String::from("enabled")),
        ] {
            let mut contents = contents.into_bytes();
            if !contents.ends_with(b"\n") {
                contents.push(b'\n');
            }
            add_entry(
                tree,
                format!("{directory}/{attribute}"),
                TreeEntry::Attribute(projected.clone(), contents),
            );
        }
        // `edid` is raw binary, not a text attribute: Linux writes exactly the
        // descriptor bytes, and `edid-decode` reads them back verbatim. A
        // trailing newline would make the byte count one past the descriptor
        // and shift every extension block.
        add_entry(
            tree,
            format!("{directory}/edid"),
            TreeEntry::Attribute(projected.clone(), connector.edid().to_vec()),
        );
    }

    // The render node is a second minor of the same card, not a second device:
    // its `dev`, `uevent`, char-device link and class link are projected from
    // the primary node's identity.
    let Some(render_minor) = node.render_device_number() else {
        return;
    };
    let render = format!("devices/{physical}/drm/renderD{}", render_minor.minor());
    add_drm_render_entries(tree, projected, &render, render_minor);
    let render_device_link = format!("{render}/device");
    add_entry(
        tree,
        render_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&render_device_link, &physical_device)),
    );
    let render_class_link = format!("class/drm/renderD{}", render_minor.minor());
    add_entry(
        tree,
        render_class_link.clone(),
        TreeEntry::Symlink(relative_symlink(&render_class_link, &render)),
    );
}

/// Projects one render-minor leaf: `dev`, `uevent`, `subsystem`, and `dev/char`.
///
/// `uevent` comes from `kdevice_uevent` so the sysfs attribute and the netlink
/// ADD/REMOVE payload for the same node are generated from one description.
fn add_drm_render_entries(
    tree: &mut BTreeMap<String, TreeEntry>,
    projected: &Arc<ProjectedDevice>,
    leaf: &str,
    number: kdevice::DeviceNumber,
) {
    let node = &projected.node;
    add_directory(tree, leaf);
    add_entry(
        tree,
        format!("{leaf}/dev"),
        TreeEntry::Attribute(
            projected.clone(),
            append_line(&format!("{}:{}", number.major(), number.minor())),
        ),
    );
    // `UeventScope::Render` makes the attribute writable (0644): a write
    // publishes the render minor's uevent through the same pipeline the
    // automatic events use.
    add_entry(
        tree,
        format!("{leaf}/uevent"),
        TreeEntry::Uevent(projected.clone(), UeventScope::Render),
    );
    let subsystem_link = format!("{leaf}/subsystem");
    add_entry(
        tree,
        subsystem_link.clone(),
        TreeEntry::Symlink(relative_symlink(
            &subsystem_link,
            &format!("class/{}", kdevice_uevent::subsystem_name(node)),
        )),
    );
    let character_device_link = format!("dev/char/{}:{}", number.major(), number.minor());
    add_entry(
        tree,
        character_device_link.clone(),
        TreeEntry::Symlink(relative_symlink(&character_device_link, leaf)),
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: the tree projection is one linear pass that builds \
              directories and leaves together; splitting it would duplicate the path walk"
)]
fn build_tree(
    nodes: &BTreeMap<kdevice::DeviceId, Arc<ProjectedDevice>>,
    characters: &[kvfs::CharacterDeviceRegistration],
) -> BTreeMap<String, TreeEntry> {
    let mut tree = BTreeMap::new();
    for directory in [
        "devices",
        "class/input",
        "class/drm",
        "dev/char",
        "fs/cgroup",
        "fs/bpf",
        "bus/pci/drivers",
        "bus/platform/drivers",
    ] {
        add_directory(&mut tree, directory);
    }

    for registration in characters {
        if !registration.is_registered() {
            continue;
        }
        let device = registration.device_id();
        let class = format!("class/{}", registration.class());
        let leaf = format!(
            "devices/virtual/{}/{}",
            registration.class(),
            registration.name()
        );
        add_directory(&mut tree, &class);
        add_directory(&mut tree, &leaf);
        add_entry(
            &mut tree,
            format!("{leaf}/dev"),
            TreeEntry::CharacterAttribute(
                registration.clone(),
                append_line(&format!("{}:{}", device.major(), device.minor())),
            ),
        );
        let properties = vec![
            ("MAJOR".to_string(), device.major().to_string()),
            ("MINOR".to_string(), device.minor().to_string()),
            ("DEVNAME".to_string(), registration.name().to_string()),
        ];
        add_entry(
            &mut tree,
            format!("{leaf}/uevent"),
            TreeEntry::RawUevent {
                path: leaf.clone(),
                subsystem: registration.class().to_string(),
                contents: render_uevent(&properties),
                properties,
            },
        );
        for (source, target) in [
            (format!("{class}/{}", registration.name()), leaf.clone()),
            (
                format!("dev/char/{}:{}", device.major(), device.minor()),
                leaf.clone(),
            ),
            (format!("{leaf}/subsystem"), class),
        ] {
            add_entry(
                &mut tree,
                source.clone(),
                TreeEntry::Symlink(relative_symlink(&source, &target)),
            );
        }
        // logind opens /sys/class/tty/tty0/active to decide whether seat0 has
        // VTs (seat_has_vts); without it CreateSession rejects VTNr. The fixed
        // VT console is always VT 1 (ktty's VT_GETSTATE reports VT 1), so
        // publish that alias as the active console.
        if registration.class() == "tty" && registration.name() == "tty0" {
            add_entry(
                &mut tree,
                format!("{leaf}/active"),
                TreeEntry::CharacterAttribute(registration.clone(), append_line("tty1")),
            );
        }
    }

    for projected in nodes.values() {
        let node = &projected.node;
        let physical = kdevice_uevent::physical_device_path(node.location());
        let bus = bus_name(node.location());
        add_directory(&mut tree, &format!("devices/{physical}"));
        add_directory(
            &mut tree,
            &format!("bus/{bus}/drivers/{}", node.driver_name()),
        );
        let subsystem_link = format!("devices/{physical}/subsystem");
        add_entry(
            &mut tree,
            subsystem_link.clone(),
            TreeEntry::Symlink(relative_symlink(&subsystem_link, &format!("bus/{bus}"))),
        );
        let driver_link = format!("devices/{physical}/driver");
        add_entry(
            &mut tree,
            driver_link.clone(),
            TreeEntry::Symlink(relative_symlink(
                &driver_link,
                &format!("bus/{bus}/drivers/{}", node.driver_name()),
            )),
        );
        // Linux publishes the PCI identity on the device directory; libdrm's
        // `drmProcessPciDevice` reads these four files to recognize a device,
        // and refuses the node (`ENODEV`) when any is missing.
        if let Some(pci) = node.pci_identity() {
            for (name, value) in [
                ("vendor", pci.vendor_id),
                ("device", pci.device_id),
                ("subsystem_vendor", pci.subsystem_vendor_id),
                ("subsystem_device", pci.subsystem_device_id),
            ] {
                add_entry(
                    &mut tree,
                    format!("devices/{physical}/{name}"),
                    TreeEntry::Attribute(projected.clone(), append_line(&format!("0x{value:04x}"))),
                );
            }
            let class = (u32::from(pci.class) << 16) | (u32::from(pci.subclass) << 8);
            add_entry(
                &mut tree,
                format!("devices/{physical}/class"),
                TreeEntry::Attribute(projected.clone(), append_line(&format!("0x{class:06x}"))),
            );
            // Linux's PCI `uevent` carries `PCI_SLOT_NAME`; libdrm parses the
            // BDF out of it when it builds a DRM device's bus info.
            let mut properties = vec![
                ("PCI_CLASS".to_string(), format!("{class:06X}")),
                (
                    "PCI_ID".to_string(),
                    format!("{:04X}:{:04X}", pci.vendor_id, pci.device_id),
                ),
                (
                    "PCI_SUBSYS_ID".to_string(),
                    format!(
                        "{:04X}:{:04X}",
                        pci.subsystem_vendor_id, pci.subsystem_device_id
                    ),
                ),
            ];
            if let DeviceLocation::Pci {
                segment,
                bus: pci_bus,
                device: pci_device,
                function,
            } = node.location()
            {
                properties.push((
                    "PCI_SLOT_NAME".to_string(),
                    format!("{segment:04x}:{pci_bus:02x}:{pci_device:02x}.{function}"),
                ));
            }
            properties.push(("DRIVER".to_string(), node.driver_name().to_string()));
            // `DEVPATH` names the device directory, never the `uevent`
            // attribute inside it; Linux's `kobject_uevent_env` reports
            // `/devices/pci0000:00/0000:00:04.0`, and udev derives its database
            // entry (`data/+pci:0000:00:04.0`) from that path. Publishing the
            // attribute path makes udev look for a device called "uevent" and
            // drop the event.
            let device_path = format!("devices/{physical}");
            add_entry(
                &mut tree,
                format!("{device_path}/uevent"),
                TreeEntry::RawUevent {
                    path: device_path,
                    subsystem: "pci".to_string(),
                    contents: render_uevent(&properties),
                    properties,
                },
            );
        } else if bus_name(node.location()) == "platform" {
            // Linux writes `MODALIAS=platform:<pdev->name>` for every platform
            // device (`drivers/base/platform.c`), and the device name is the
            // last component of its sysfs path. The projected MMIO and static
            // devices are named after that path leaf (`a003e00.mmio`,
            // `static9001`), so the leaf is the platform device name.
            //
            // libdrm's `drmParseOFBusInfo` falls back to this attribute when it
            // builds a DRM platform bus info and returns `ENOENT` without it, so
            // `drmGetDevices2` silently drops the node and clients such as
            // `kmscube` report "no drm device found".
            let leaf = physical.rsplit('/').next().unwrap_or(physical.as_str());
            let properties = vec![
                ("DRIVER".to_string(), node.driver_name().to_string()),
                ("MODALIAS".to_string(), format!("platform:{leaf}")),
            ];
            let path = format!("devices/{physical}/uevent");
            add_entry(
                &mut tree,
                path.clone(),
                TreeEntry::RawUevent {
                    path,
                    subsystem: "platform".to_string(),
                    contents: render_uevent(&properties),
                    properties,
                },
            );
        }
        match node.kind() {
            DeviceNodeKind::Input => add_input_entries(&mut tree, projected),
            DeviceNodeKind::Drm => add_drm_entries(&mut tree, projected),
        }
    }
    tree
}

struct SysfsDirectory {
    fs: Arc<SimpleFs>,
    path: String,
}

impl DirectoryEntry {
    fn inode(&self, fs: Arc<SimpleFs>, path: String) -> Arc<kvfs::VfsInode> {
        self.inode
            .call_once(|| {
                SimpleDir::new_inode_with_owner(
                    fs.clone(),
                    Arc::new(SysfsDirectory { fs, path }),
                    NodePermission::from_bits_truncate(0o755),
                    0,
                    0,
                )
            })
            .clone()
    }
}

impl SysfsDirectory {
    fn child_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_string()
        } else {
            format!("{}/{name}", self.path)
        }
    }
}

impl SimpleDirOps for SysfsDirectory {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let prefix = if self.path.is_empty() {
            String::new()
        } else {
            format!("{}/", self.path)
        };
        let mut names = BTreeMap::<String, ()>::new();
        let projection = device_projection().lock();
        for path in projection.tree.keys() {
            let Some(remainder) = path.strip_prefix(&prefix) else {
                continue;
            };
            let name = remainder.split('/').next().unwrap_or(remainder);
            if !name.is_empty() {
                names.insert(name.to_string(), ());
            }
        }
        drop(projection);
        let names: Vec<Cow<'a, str>> = names.into_keys().map(Cow::Owned).collect();
        Ok(KBox::new(names.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let path = self.child_path(name);
        let entry = device_projection()
            .lock()
            .tree
            .get(&path)
            .cloned()
            .ok_or(VfsError::NotFound)?;
        match entry {
            TreeEntry::CharacterAttribute(registration, contents) => lookup.file(
                name,
                SimpleFile::new_regular_with_permission(
                    self.fs.clone(),
                    READ_ONLY_PERMISSION,
                    move || {
                        if registration.is_registered() {
                            Ok(contents.clone())
                        } else {
                            Err(VfsError::NoSuchDevice)
                        }
                    },
                ),
            ),
            TreeEntry::Directory(directory) => {
                Ok(lookup.dir_from_inode(name, directory.inode(self.fs.clone(), path)))
            }
            TreeEntry::Attribute(projected, contents) => lookup.file(
                name,
                SimpleFile::new_regular_with_permission(
                    self.fs.clone(),
                    READ_ONLY_PERMISSION,
                    move || projected.with_available(|_| contents.clone()),
                ),
            ),
            TreeEntry::Symlink(target) => lookup.file(
                name,
                SimpleFile::new(self.fs.clone(), NodeType::Symlink, move || {
                    Ok(target.clone())
                }),
            ),
            TreeEntry::Uevent(projected, scope) => {
                let file = CommandFile::new(move |operation| -> VfsResult<Option<Vec<u8>>> {
                    match operation {
                        SimpleFileOperation::Read => projected
                            .with_available(|node| {
                                kdevice_uevent::uevent_file_contents(node, scope)
                            })
                            .map(Some),
                        SimpleFileOperation::Write { data, .. } => {
                            projected
                                .with_available(|node| -> VfsResult<()> {
                                    // `kobject_uevent_store` parses the whole
                                    // attribute text before it publishes.
                                    let action = parse_uevent_action(data)?;
                                    kdevice_uevent::publish(action, node, scope);
                                    Ok(())
                                })
                                .and_then(|result| result)
                                .map(|()| None)
                        }
                    }
                });
                lookup.file(
                    name,
                    SimpleFile::new_regular_with_permission(
                        self.fs.clone(),
                        UEVENT_PERMISSION,
                        file,
                    ),
                )
            }
            TreeEntry::RawUevent {
                path,
                subsystem,
                properties,
                contents,
            } => {
                let file = CommandFile::new(move |operation| -> VfsResult<Option<Vec<u8>>> {
                    match operation {
                        SimpleFileOperation::Read => Ok(Some(contents.clone())),
                        SimpleFileOperation::Write { data, .. } => {
                            // The device-directory uevent parses its action
                            // with the same rule as the device one.
                            let action = parse_uevent_action(data)?;
                            kdevice_uevent::publish_raw(action, &path, &subsystem, &properties);
                            Ok(None)
                        }
                    }
                });
                lookup.file(
                    name,
                    SimpleFile::new_regular_with_permission(
                        self.fs.clone(),
                        UEVENT_PERMISSION,
                        file,
                    ),
                )
            }
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
