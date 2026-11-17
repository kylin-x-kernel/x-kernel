// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host integration for Linux-compatible device-node uevents.
//!
//! The device model owns lifecycle ordering. This crate translates its ordered
//! notifications into kobject netlink messages and shares the same formatter
//! with sysfs `uevent` attributes.

#![no_std]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kdevice::{
    DRM_RENDER_MINOR_BASE, DeviceLocation, DeviceNode, DeviceNodeKind, DeviceNodeLifecycleEvent,
    install_device_node_lifecycle_publisher,
};
use klazy::Once;

const UEVENT_GROUP: u32 = 1;

static INITIALIZATION: Once<()> = Once::new();

/// Device object whose properties and path are represented by one event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UeventScope {
    /// The character device leaf, such as `event0` or `card0`.
    Device,
    /// The Linux input parent object, such as `input0`.
    InputParent,
    /// The render minor of a DRM card, such as `renderD128`.
    Render,
}

/// Valid Linux kobject event action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UeventAction {
    /// Object creation.
    Add,
    /// Object removal.
    Remove,
    /// Object state or attribute change.
    Change,
    /// Object path change.
    Move,
    /// Object became available.
    Online,
    /// Object became unavailable.
    Offline,
    /// Driver binding.
    Bind,
    /// Driver unbinding.
    Unbind,
}

impl UeventAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Change => "change",
            Self::Move => "move",
            Self::Online => "online",
            Self::Offline => "offline",
            Self::Bind => "bind",
            Self::Unbind => "unbind",
        }
    }
}

/// The input does not name a supported uevent action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidUeventAction;

impl TryFrom<&str> for UeventAction {
    type Error = InvalidUeventAction;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "add" => Ok(Self::Add),
            "remove" => Ok(Self::Remove),
            "change" => Ok(Self::Change),
            "move" => Ok(Self::Move),
            "online" => Ok(Self::Online),
            "offline" => Ok(Self::Offline),
            "bind" => Ok(Self::Bind),
            "unbind" => Ok(Self::Unbind),
            _ => Err(InvalidUeventAction),
        }
    }
}

/// Initializes automatic device-node lifecycle publication.
///
/// This must run before device discovery starts. The device model rejects a
/// late installation so boot cannot silently omit earlier add events.
pub fn init() {
    INITIALIZATION.call_once(|| {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Boot installs the uevent publisher \
                      exactly once before discovery, as required by the device lifecycle contract"
        )]
        install_device_node_lifecycle_publisher(Arc::new(handle_lifecycle_event))
            .expect("device-node uevent publisher must initialize before device discovery");
    });
}

fn handle_lifecycle_event(event: DeviceNodeLifecycleEvent) {
    match event {
        DeviceNodeLifecycleEvent::Added(node) => publish_add(&node),
        DeviceNodeLifecycleEvent::Removed(node) => publish_remove(&node),
    }
}

/// Returns the canonical sysfs physical path for a device location.
pub fn physical_device_path(location: DeviceLocation) -> String {
    match location {
        DeviceLocation::Pci {
            segment,
            bus,
            device,
            function,
        } => format!("pci{segment:04x}:{bus:02x}/{segment:04x}:{bus:02x}:{device:02x}.{function}"),
        DeviceLocation::Mmio { base, .. } => format!("platform/{base:x}.mmio"),
        DeviceLocation::FirmwareNode { id } => format!("platform/firmware{id}"),
        DeviceLocation::PlatformStatic { id } => format!("platform/static{id}"),
        DeviceLocation::Bridge { domain } => format!("pci{domain:04x}:00"),
    }
}

fn uevent_path(node: &DeviceNode, scope: UeventScope) -> Option<String> {
    let physical = physical_device_path(node.location());
    match (node.kind(), scope) {
        (DeviceNodeKind::Input, UeventScope::Device) => Some(format!(
            "devices/{physical}/input/input{0}/event{0}",
            node.index()
        )),
        (DeviceNodeKind::Input, UeventScope::InputParent) => {
            Some(format!("devices/{physical}/input/input{}", node.index()))
        }
        (DeviceNodeKind::Drm, UeventScope::Device) => {
            Some(format!("devices/{physical}/drm/card{}", node.index()))
        }
        (DeviceNodeKind::Drm, UeventScope::Render) => Some(format!(
            "devices/{physical}/drm/renderD{}",
            DRM_RENDER_MINOR_BASE + node.index()
        )),
        // Input nodes have no render minor and DRM nodes have no input parent;
        // both combinations are structurally impossible call sites.
        (DeviceNodeKind::Input, UeventScope::Render)
        | (DeviceNodeKind::Drm, UeventScope::InputParent) => None,
    }
}

/// Returns the Linux subsystem name for a projected device node.
pub const fn subsystem_name(node: &DeviceNode) -> &'static str {
    match node.kind() {
        DeviceNodeKind::Input => "input",
        DeviceNodeKind::Drm => "drm",
    }
}

fn sanitized_property(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Formats a little-endian capability bitmap using Linux sysfs word order.
///
/// Bytes are grouped into native 64-bit words, rendered with the highest
/// non-zero word first, and separated by spaces. An empty or all-zero bitmap
/// is represented as `0`.
pub fn bitmap_value(bytes: &[u8]) -> String {
    let words: Vec<u64> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut raw = [0u8; 8];
            raw[..chunk.len()].copy_from_slice(chunk);
            u64::from_le_bytes(raw)
        })
        .collect();
    if words.is_empty() {
        return "0".to_string();
    }
    let last = words.iter().rposition(|word| *word != 0).unwrap_or(0);
    words[..=last]
        .iter()
        .rev()
        .map(|word| format!("{word:x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Returns the properties represented by a sysfs `uevent` file or message.
pub fn uevent_properties(node: &DeviceNode, scope: UeventScope) -> Vec<(String, String)> {
    if matches!(scope, UeventScope::InputParent) {
        let Some(metadata) = node.input_metadata() else {
            return Vec::new();
        };
        let id = metadata.device_id();
        let mut properties = alloc::vec![
            (
                "PRODUCT".to_string(),
                format!(
                    "{:x}/{:x}/{:x}/{:x}",
                    id.bus_type, id.vendor, id.product, id.version
                ),
            ),
            ("NAME".to_string(), sanitized_property(metadata.name())),
            (
                "PHYS".to_string(),
                sanitized_property(metadata.physical_location()),
            ),
            ("PROP".to_string(), "0".to_string()),
            ("EV".to_string(), bitmap_value(metadata.event_types())),
        ];
        if !metadata.unique_id().is_empty() {
            properties.push(("UNIQ".to_string(), sanitized_property(metadata.unique_id())));
        }
        for (event_type, name) in [
            (1, "KEY"),
            (2, "REL"),
            (3, "ABS"),
            (4, "MSC"),
            (5, "SW"),
            (0x11, "LED"),
            (0x12, "SND"),
            (0x15, "FF"),
        ] {
            if let Some(bits) = metadata.event_codes(event_type) {
                properties.push((name.to_string(), bitmap_value(bits)));
            }
        }
        return properties;
    }

    if node.kind() == DeviceNodeKind::Drm && scope == UeventScope::Render {
        return render_uevent_properties(node).unwrap_or_default();
    }

    let number = node.device_number();
    let mut properties = alloc::vec![
        ("MAJOR".to_string(), number.major().to_string()),
        ("MINOR".to_string(), number.minor().to_string()),
        ("DEVNAME".to_string(), node.devname().to_string()),
    ];
    if node.kind() == DeviceNodeKind::Drm {
        properties.push(("DEVTYPE".to_string(), "drm_minor".to_string()));
    }
    properties
}

/// Renders the newline-delimited contents of a sysfs `uevent` attribute.
pub fn uevent_file_contents(node: &DeviceNode, scope: UeventScope) -> Vec<u8> {
    properties_to_uevent(&uevent_properties(node, scope))
}

fn properties_to_uevent(properties: &[(String, String)]) -> Vec<u8> {
    let mut contents = String::new();
    for (key, value) in properties {
        contents.push_str(key);
        contents.push('=');
        contents.push_str(value);
        contents.push('\n');
    }
    contents.into_bytes()
}

/// Returns the sysfs path of the render node belonging to a DRM node.
///
/// This is the same `128 + card index` projection devfs registers as
/// `/dev/dri/renderD*`; both derive it from [`DeviceNode`] so the two views
/// cannot disagree.
pub fn render_uevent_path(node: &DeviceNode) -> Option<String> {
    let devname = node.render_devname()?;
    let physical = physical_device_path(node.location());
    Some(format!(
        "devices/{physical}/drm/{}",
        devname.rsplit('/').next()?
    ))
}

/// Returns the properties of a DRM render node's `uevent` attribute.
pub fn render_uevent_properties(node: &DeviceNode) -> Option<Vec<(String, String)>> {
    let number = node.render_device_number()?;
    let devname = node.render_devname()?;
    Some(alloc::vec![
        ("MAJOR".to_string(), number.major().to_string()),
        ("MINOR".to_string(), number.minor().to_string()),
        ("DEVNAME".to_string(), devname),
        ("DEVTYPE".to_string(), "drm_minor".to_string()),
    ])
}

/// Publishes one action for a device object.
///
/// Lifecycle add/remove actions are generated internally by [`init`]. An
/// input-parent scope for a non-input device is ignored.
pub fn publish(action: UeventAction, node: &DeviceNode, scope: UeventScope) {
    if scope == UeventScope::InputParent && node.kind() != DeviceNodeKind::Input {
        return;
    }
    let Some(path) = uevent_path(node, scope) else {
        return;
    };
    publish_object(
        action,
        &path,
        subsystem_name(node),
        &uevent_properties(node, scope),
    );
}

/// Publishes a userspace-requested uevent for a sysfs object that the device
/// model does not represent as an input/DRM [`DeviceNode`].
pub fn publish_raw(
    action: UeventAction,
    path: &str,
    subsystem: &str,
    properties: &[(String, String)],
) {
    publish_object(action, path, subsystem, properties);
}

fn publish_object(
    action: UeventAction,
    path: &str,
    subsystem: &str,
    properties: &[(String, String)],
) {
    let action = action.as_str();
    let devpath = format!("/{path}");
    let mut payload = format!("{action}@{devpath}\0ACTION={action}\0DEVPATH={devpath}\0");
    payload.push_str("SUBSYSTEM=");
    payload.push_str(subsystem);
    payload.push('\0');
    for (key, value) in properties {
        payload.push_str(key);
        payload.push('=');
        payload.push_str(value);
        payload.push('\0');
    }
    knet::netlink::publish_kobject_uevent(UEVENT_GROUP, payload.as_bytes());
}

/// Publishes a DRM connector hotplug notification for a card node.
///
/// Linux sends `KOBJ_CHANGE` with `HOTPLUG=1` on the card's kobject when a
/// connector's mode set changes, and userspace re-reads the connector in
/// response. This is the same notification on the same object: the card is
/// the only DRM kobject the device model publishes, so no connector object is
/// required for a client to re-probe.
pub fn publish_drm_hotplug(node: &DeviceNode) {
    if node.kind() != DeviceNodeKind::Drm {
        return;
    }
    let Some(path) = uevent_path(node, UeventScope::Device) else {
        return;
    };
    let mut properties = uevent_properties(node, UeventScope::Device);
    // The Linux kernel adds this to the uevent environment so a listener can
    // tell a connector change from an unrelated change on the same object.
    properties.push(("HOTPLUG".to_string(), "1".to_string()));
    publish_object(
        UeventAction::Change,
        &path,
        subsystem_name(node),
        &properties,
    );
}

/// Publishes the render minor of a DRM node when the node has one.
fn publish_render(action: UeventAction, node: &DeviceNode) {
    publish(action, node, UeventScope::Render);
}

/// Publishes the render minor's uevent on user request (a write to the sysfs
/// `uevent` attribute).
pub fn publish_render_uevent(action: UeventAction, node: &DeviceNode) {
    publish_render(action, node);
}

/// Publishes the render minor's uevent on user request (a write to the sysfs
/// `uevent` attribute).
fn publish_add(node: &DeviceNode) {
    if node.kind() == DeviceNodeKind::Input {
        publish(UeventAction::Add, node, UeventScope::InputParent);
    }
    publish(UeventAction::Add, node, UeventScope::Device);
    publish_render(UeventAction::Add, node);
}

fn publish_remove(node: &DeviceNode) {
    publish_render(UeventAction::Remove, node);
    publish(UeventAction::Remove, node, UeventScope::Device);
    if node.kind() == DeviceNodeKind::Input {
        publish(UeventAction::Remove, node, UeventScope::InputParent);
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
