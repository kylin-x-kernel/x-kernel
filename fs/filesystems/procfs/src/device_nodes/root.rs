// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/proc/devices`, `/proc/bus/devices`, `/proc/bus/drivers` — device topology export.

use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt::Write;

use kdevice::{DeviceIdentity, DeviceKind, TransportInfo};
use kvfs::{DirMapping, SeqFileInode, SeqIterator, SimpleDir, SimpleFs};

// ---------------------------------------------------------------------------
// /proc/devices — all devices grouped by kind
// ---------------------------------------------------------------------------

pub struct DevicesIter {
    text: String,
    done: bool,
}

impl DevicesIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for DevicesIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let topo = kdevice::device_topology();

        let mut block = Vec::new();
        let mut char_dev = Vec::new();
        let mut net = Vec::new();
        let mut display = Vec::new();
        let mut input = Vec::new();
        let mut other = Vec::new();

        for device in topo.devices() {
            let kind = device.record.device_kind.unwrap_or(DeviceKind::Block);
            let driver = device.record.driver_name.unwrap_or("<none>");
            let state = device.record.state.as_str();
            let line = format!(
                "  {:>3} {:<12} {} [{}]\n",
                device.record.id.raw(),
                kind.as_str(),
                driver,
                state,
            );

            if kind == DeviceKind::Block {
                block.push(line);
            } else if kind == DeviceKind::Char {
                char_dev.push(line);
            } else if kind == DeviceKind::Net {
                net.push(line);
            } else if kind == DeviceKind::Display {
                display.push(line);
            } else if kind == DeviceKind::Input {
                input.push(line);
            } else {
                other.push(line);
            }
        }

        let s = &mut self.text;
        if !block.is_empty() {
            s.push_str("Block devices:\n");
            for l in &block {
                s.push_str(l);
            }
        }
        if !char_dev.is_empty() {
            s.push_str("Character devices:\n");
            for l in &char_dev {
                s.push_str(l);
            }
        }
        if !net.is_empty() {
            s.push_str("Network devices:\n");
            for l in &net {
                s.push_str(l);
            }
        }
        if !display.is_empty() {
            s.push_str("Display devices:\n");
            for l in &display {
                s.push_str(l);
            }
        }
        if !input.is_empty() {
            s.push_str("Input devices:\n");
            for l in &input {
                s.push_str(l);
            }
        }
        if !other.is_empty() {
            s.push_str("Other devices:\n");
            for l in &other {
                s.push_str(l);
            }
        }

        if s.is_empty() {
            s.push_str("No devices registered.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// /proc/bus/devices — all devices grouped by bus
// ---------------------------------------------------------------------------

pub struct BusDevicesIter {
    text: String,
    done: bool,
}

impl BusDevicesIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for BusDevicesIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let topo = kdevice::device_topology();
        let s = &mut self.text;

        for bus in topo.buses() {
            let bus_name = bus.info.name;
            let bus_id = bus.info.id;

            for device in topo.devices_on_bus(bus_id) {
                let identity = format_identity(&device.record.identity, device.record.transport);
                let driver = device.record.driver_name.unwrap_or("<none>");
                let state = device.record.state.as_str();
                let _ = writeln!(
                    s,
                    "{:<12} {:>3} {} [{}] driver={}",
                    bus_name,
                    device.record.id.raw(),
                    identity,
                    state,
                    driver,
                );
            }
        }

        if s.is_empty() {
            s.push_str("No devices on any bus.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// /proc/bus/drivers — all drivers grouped by bus type
// ---------------------------------------------------------------------------

pub struct BusDriversIter {
    text: String,
    done: bool,
}

impl BusDriversIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for BusDriversIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let topo = kdevice::device_topology();
        let s = &mut self.text;

        for driver in topo.drivers() {
            let _ = writeln!(
                s,
                "{:<16} [{}]",
                driver.info.name,
                driver.info.device_kind.as_str(),
            );
        }

        if s.is_empty() {
            s.push_str("No drivers registered.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// /proc/bus/topology — bus/device tree with parent-child relations
// ---------------------------------------------------------------------------

pub struct BusTopologyIter {
    text: String,
    done: bool,
}

impl BusTopologyIter {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            done: false,
        }
    }
}

impl SeqIterator for BusTopologyIter {
    type Item = String;

    fn rewind(&mut self) {
        self.text.clear();
        self.done = false;

        let topo = kdevice::device_topology();
        let s = &mut self.text;

        // Build a map from BusId raw → controller device view.
        let mut bus_controller: alloc::collections::BTreeMap<u64, kdevice::DeviceRecord> =
            alloc::collections::BTreeMap::new();
        for device in topo.devices() {
            if let Some(child_bus) = device.record.child_bus {
                bus_controller.insert(child_bus.raw(), device.record.clone());
            }
        }

        // Print each bus with its devices, showing parent/child_bus.
        for bus in topo.buses() {
            let bus_id = bus.info.id;
            let bus_name = bus.info.name;

            // Show controller if any.
            if let Some(ctrl) = bus_controller.get(&bus_id.raw()) {
                let ctrl_id = format_identity(&ctrl.identity, ctrl.transport);
                let ctrl_driver = ctrl.driver_name.unwrap_or("<none>");
                let _ = writeln!(
                    s,
                    "{:<16} controller: {} {} [{}] driver={}",
                    bus_name,
                    ctrl.id.raw(),
                    ctrl_id,
                    ctrl.state.as_str(),
                    ctrl_driver,
                );
            } else {
                let _ = writeln!(s, "{:<16} (no controller)", bus_name);
            }

            // Show devices on this bus.
            for device in topo.devices_on_bus(bus_id) {
                let identity = format_identity(&device.record.identity, device.record.transport);
                let driver = device.record.driver_name.unwrap_or("<none>");
                let state = device.record.state.as_str();

                let parent_str = match device.record.parent {
                    Some(pid) => alloc::format!("parent={}", pid.raw()),
                    None => String::from("parent=<root>"),
                };
                let child_bus_str = match device.record.child_bus {
                    Some(cbid) => alloc::format!("child_bus={}", cbid.raw()),
                    None => String::new(),
                };

                let _ = write!(
                    s,
                    "  {:>3} {} [{}] driver={} {}",
                    device.record.id.raw(),
                    identity,
                    state,
                    driver,
                    parent_str,
                );
                if !child_bus_str.is_empty() {
                    let _ = write!(s, " {}", child_bus_str);
                }
                s.push('\n');
            }
        }

        if s.is_empty() {
            s.push_str("No buses registered.\n");
        }
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.done = true;
        Some(core::mem::take(&mut self.text))
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        buf.push_str(item);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_identity(identity: &DeviceIdentity, transport: Option<TransportInfo>) -> String {
    let virtio_type = transport.map(|TransportInfo::Virtio { device_type }| device_type);
    match identity {
        DeviceIdentity::Pci(pci) => match virtio_type {
            Some(device_type) => format!(
                "pci:{:04x}:{:04x}:virtio:{}",
                pci.vendor_id, pci.device_id, device_type
            ),
            None => format!("pci:{:04x}:{:04x}", pci.vendor_id, pci.device_id),
        },
        DeviceIdentity::Platform(platform) => {
            if let Some(device_type) = virtio_type {
                format!("virtio-mmio:{}", device_type)
            } else if let Some(alias) = platform.alias {
                format!("compatible:{}", alias)
            } else if let Some(id) = platform.firmware_id {
                format!("firmware:{}", id)
            } else {
                String::from("platform:unknown")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "devices",
        SeqFileInode::new_regular(fs.clone(), DevicesIter::new),
    );
    root.add("bus", {
        let mut bus = DirMapping::new();
        bus.add(
            "devices",
            SeqFileInode::new_regular(fs.clone(), BusDevicesIter::new),
        );
        bus.add(
            "drivers",
            SeqFileInode::new_regular(fs.clone(), BusDriversIter::new),
        );
        bus.add(
            "topology",
            SeqFileInode::new_regular(fs.clone(), BusTopologyIter::new),
        );
        SimpleDir::new_maker(fs.clone(), Arc::new(bus))
    });
}
