// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dynamic devtmpfs projection for device-core node assignments.

use alloc::{borrow::Cow, boxed::Box, collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use kclass::find_display_device;
#[cfg(feature = "input")]
use kclass::find_input_device;
use kdevice::{
    DeviceId as CoreDeviceId, DeviceNode, DeviceNodeEvent, DeviceNodeKind, subscribe_device_nodes,
};
use klazy::Once;
use ksync::Mutex;
use kvfs::{
    Dentry, DeviceId, SimpleDirLookup, SimpleDirOps, SimpleFs, VfsError, VfsResult, cdev_del,
};
use lazyinit::LazyInit;

#[cfg(feature = "input")]
use crate::nodes::event::EventDev;
use crate::{DeviceFile, device_dentry};

type DeviceRemovalCallback = Arc<dyn Fn() + Send + Sync>;

struct DeviceNodeRecord {
    node: DeviceNode,
    file: Arc<DeviceFile>,
    on_remove: DeviceRemovalCallback,
}

struct DeviceProjection {
    fs: Arc<SimpleFs>,
    records: BTreeMap<CoreDeviceId, DeviceNodeRecord>,
}

impl DeviceProjection {
    fn new(fs: Arc<SimpleFs>) -> Self {
        Self {
            fs,
            records: BTreeMap::new(),
        }
    }
}

static DEVICES: LazyInit<Mutex<DeviceProjection>> = LazyInit::new();
static DEVICE_INITIALIZATION: Once<()> = Once::new();

fn projection() -> &'static Mutex<DeviceProjection> {
    DEVICES
        .get()
        .expect("dynamic devtmpfs projection must initialize with its filesystem")
}

#[cfg(feature = "input")]
fn input_file(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    id: CoreDeviceId,
) -> VfsResult<Option<(Arc<DeviceFile>, DeviceRemovalCallback)>> {
    let Some(device) = find_input_device(id) else {
        return Ok(None);
    };
    let endpoint = match inputdev::input_endpoint(device.clone()) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            warn!("devtmpfs: input endpoint for {id:?} failed: {error:?}");
            return Ok(None);
        }
    };
    let event = Arc::new(EventDev::new(device, endpoint));
    let weak_event = Arc::downgrade(&event);
    let on_remove: DeviceRemovalCallback = Arc::new(move || {
        if let Some(event) = weak_event.upgrade() {
            event.detach();
        }
    });
    Ok(Some((
        DeviceFile::new_character(fs, number, event)?,
        on_remove,
    )))
}

fn display_file(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    id: CoreDeviceId,
) -> VfsResult<Option<(Arc<DeviceFile>, DeviceRemovalCallback)>> {
    let Some(device) = find_display_device(id) else {
        return Ok(None);
    };
    let card = drmdevice::DrmCard::new(device);
    let weak_card = Arc::downgrade(&card);
    let on_remove: DeviceRemovalCallback = Arc::new(move || {
        if let Some(card) = weak_card.upgrade() {
            card.notify_removed();
        }
    });
    Ok(Some((
        DeviceFile::new_character(fs, number, card)?,
        on_remove,
    )))
}

fn add_device(projection: &mut DeviceProjection, node: DeviceNode) {
    if projection.records.contains_key(&node.core_id()) {
        return;
    }
    let number = node.device_number();
    let number = DeviceId::new(number.major(), number.minor());
    let file = match node.kind() {
        #[cfg(feature = "input")]
        DeviceNodeKind::Input => input_file(projection.fs.clone(), number, node.core_id()),
        #[cfg(not(feature = "input"))]
        DeviceNodeKind::Input => Ok(None),
        DeviceNodeKind::Drm => display_file(projection.fs.clone(), number, node.core_id()),
    };
    let file = match file {
        Ok(file) => file,
        Err(error) => {
            warn!(
                "devtmpfs: cannot register device {:?}: {error:?}",
                node.core_id()
            );
            return;
        }
    };
    let Some((file, on_remove)) = file else {
        warn!(
            "devtmpfs: class device for node {:?} is unavailable",
            node.core_id()
        );
        return;
    };
    projection.records.insert(
        node.core_id(),
        DeviceNodeRecord {
            node,
            file,
            on_remove,
        },
    );
}

fn remove_device(node: &DeviceNode) {
    let record = {
        let mut projection = projection().lock();
        let Some(record) = projection.records.get(&node.core_id()) else {
            return;
        };
        if record.node != *node {
            return;
        }
        projection
            .records
            .remove(&node.core_id())
            .expect("checked device projection must remain present")
    };
    (record.on_remove)();
    let number = node.device_number();
    if cdev_del(DeviceId::new(number.major(), number.minor()), 1).is_none() {
        warn!("devtmpfs: missing character device during removal: {number:?}");
    }
}

fn handle_device_event(event: DeviceNodeEvent) {
    match event {
        DeviceNodeEvent::Added(node) => add_device(&mut projection().lock(), node),
        DeviceNodeEvent::Removing(_) => {}
        DeviceNodeEvent::Removed(node) => remove_device(&node),
    }
}

pub(crate) fn initialize(fs: Arc<SimpleFs>) {
    DEVICE_INITIALIZATION.call_once(|| {
        DEVICES.init_once(Mutex::new(DeviceProjection::new(fs)));

        let callback = Arc::new(handle_device_event);
        let mut projection = projection().lock();
        let nodes = subscribe_device_nodes(callback);
        for node in nodes {
            add_device(&mut projection, node);
        }
    });
}

#[derive(Clone, Copy)]
pub(crate) struct DynamicDeviceDir {
    kind: DeviceNodeKind,
}

impl DynamicDeviceDir {
    pub(crate) const fn input() -> Self {
        Self {
            kind: DeviceNodeKind::Input,
        }
    }

    pub(crate) const fn drm() -> Self {
        Self {
            kind: DeviceNodeKind::Drm,
        }
    }

    fn local_name<'a>(&self, node: &'a DeviceNode) -> Option<&'a str> {
        if node.kind() != self.kind {
            return None;
        }
        node.devname().rsplit('/').next()
    }
}

impl SimpleDirOps for DynamicDeviceDir {
    fn child_names<'a>(&'a self) -> VfsResult<Box<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = projection()
            .lock()
            .records
            .values()
            .filter_map(|record| self.local_name(&record.node))
            .map(|name| Cow::Owned(String::from(name)))
            .collect();
        Ok(Box::new(names.into_iter()))
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let file = projection()
            .lock()
            .records
            .values()
            .find(|record| self.local_name(&record.node) == Some(name))
            .map(|record| record.file.clone())
            .ok_or(VfsError::NotFound)?;
        Ok(device_dentry(lookup, name, file))
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}
