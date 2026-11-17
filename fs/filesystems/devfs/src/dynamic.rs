// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dynamic devtmpfs projection for device-core node assignments.

use alloc::{borrow::Cow, collections::BTreeMap, string::String, sync::Arc, vec::Vec};

use kalloc_types::KBox;
use kclass::find_display_publication;
#[cfg(feature = "input")]
use kclass::find_input_publication;
use kdevice::{
    DeviceNode, DeviceNodeEvent, DeviceNodeKey, DeviceNodeKind, PublicationId,
    subscribe_device_nodes,
};
use klazy::Once;
use ksync::Mutex;
use kvfs::{
    Dentry, DeviceId, DirMapping, LockedDentry, SimpleDir, SimpleDirLookup, SimpleDirOps, SimpleFs,
    Umode, VfsError, VfsInode, VfsResult, cdev_del,
};
use lazyinit::LazyInit;

#[cfg(feature = "input")]
use crate::nodes::event::EventDev;
use crate::{DeviceFile, content, device_dentry};

type DeviceRemovalCallback = Arc<dyn Fn() + Send + Sync>;

/// Files projected for one device node: its primary node, the render minor of a
/// DRM node when it has one, and the removal hook shared by both.
type NodeFiles = (
    Arc<DeviceFile>,
    Option<Arc<DeviceFile>>,
    DeviceRemovalCallback,
);

struct DeviceNodeRecord {
    node: DeviceNode,
    file: Arc<DeviceFile>,
    /// Render minor of a DRM node. Both files drive the same card object.
    render_file: Option<Arc<DeviceFile>>,
    on_remove: DeviceRemovalCallback,
}

struct DeviceProjection {
    fs: Arc<SimpleFs>,
    records: BTreeMap<DeviceNodeKey, DeviceNodeRecord>,
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
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Filesystem initialization installs the \
                  permanent devtmpfs projection before device callbacks"
    )]
    DEVICES
        .get()
        .expect("dynamic devtmpfs projection must initialize with its filesystem")
}

#[cfg(feature = "input")]
fn input_file(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    publication: PublicationId,
) -> VfsResult<Option<(Arc<DeviceFile>, DeviceRemovalCallback)>> {
    let Some(device) = find_input_publication(publication) else {
        return Ok(None);
    };
    let endpoint = match inputdev::input_endpoint(device.clone()) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            warn!("devtmpfs: input endpoint for {publication:?} failed: {error:?}");
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

fn display_files(
    fs: Arc<SimpleFs>,
    number: DeviceId,
    render: Option<DeviceId>,
    node: DeviceNode,
) -> VfsResult<Option<NodeFiles>> {
    let Some(device) = find_display_publication(node.publication_id()) else {
        return Ok(None);
    };
    let card = drmdevice::DrmDevice::new(device);
    // A host resize is only visible to a client that re-reads the connector,
    // so announce it the Linux way: a `change` uevent carrying HOTPLUG=1 on
    // the card node, which is what udev forwards to the compositor.
    let hotplug_node = node.clone();
    card.set_hotplug_sink(Arc::new(move || {
        kdevice_uevent::publish_drm_hotplug(&hotplug_node);
    }));
    let weak_card = Arc::downgrade(&card);
    let on_remove: DeviceRemovalCallback = Arc::new(move || {
        if let Some(card) = weak_card.upgrade() {
            card.notify_removed();
        }
    });
    let file = DeviceFile::new_character(
        fs.clone(),
        number,
        card.minor(drmdevice::MinorKind::Primary, number.minor()),
    )?;
    // The render node is a second minor over the same `DrmCard`: it shares the
    // GEM/KMS objects and differs only in the per-open file state and in the
    // ioctl allowlist enforced by the driver.
    let render_file = match render {
        Some(render) => {
            let unit = render.minor();
            match DeviceFile::new_character(
                fs,
                render,
                card.minor(drmdevice::MinorKind::Render, unit),
            ) {
                Ok(file) => Some(file),
                Err(error) => {
                    // Do not leave the primary character device registered without
                    // a projection record: that would make it unremovable.
                    let _ = cdev_del(number, 1);
                    return Err(error);
                }
            }
        }
        None => None,
    };
    Ok(Some((file, render_file, on_remove)))
}

fn add_device(projection: &mut DeviceProjection, node: DeviceNode) {
    if projection.records.contains_key(&node.key()) {
        return;
    }
    let number = node.device_number();
    let number = DeviceId::new(number.major(), number.minor());
    let render = node
        .render_device_number()
        .map(|number| DeviceId::new(number.major(), number.minor()));
    let file = match node.kind() {
        #[cfg(feature = "input")]
        DeviceNodeKind::Input => input_file(projection.fs.clone(), number, node.publication_id())
            .map(|file| file.map(|(file, on_remove)| (file, None, on_remove))),
        #[cfg(not(feature = "input"))]
        DeviceNodeKind::Input => Ok(None),
        DeviceNodeKind::Drm => display_files(projection.fs.clone(), number, render, node.clone()),
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
    let Some((file, render_file, on_remove)) = file else {
        warn!(
            "devtmpfs: class device for node {:?} is unavailable",
            node.core_id()
        );
        return;
    };
    projection.records.insert(
        node.key(),
        DeviceNodeRecord {
            node,
            file,
            render_file,
            on_remove,
        },
    );
}

fn remove_device(node: &DeviceNode) {
    let record = {
        let mut projection = projection().lock();
        let Some(record) = projection.records.get(&node.key()) else {
            return;
        };
        if record.node != *node {
            return;
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The same locked projection map was \
                      checked for this node immediately before removal"
        )]
        projection
            .records
            .remove(&node.key())
            .expect("checked device projection must remain present")
    };
    (record.on_remove)();
    let number = node.device_number();
    if cdev_del(DeviceId::new(number.major(), number.minor()), 1).is_none() {
        warn!("devtmpfs: missing character device during removal: {number:?}");
    }
    if record.render_file.is_some()
        && let Some(render) = node.render_device_number()
    {
        let render = DeviceId::new(render.major(), render.minor());
        if cdev_del(render, 1).is_none() {
            warn!("devtmpfs: missing DRM render device during removal: {render:?}");
        }
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

pub(crate) struct WritableDir {
    fs: Arc<SimpleFs>,
    mapping: Mutex<DirMapping>,
}

impl WritableDir {
    pub(crate) fn new(fs: Arc<SimpleFs>, mapping: DirMapping) -> Self {
        Self {
            fs,
            mapping: Mutex::new(mapping),
        }
    }
}

impl SimpleDirOps for WritableDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let names: Vec<Cow<'a, str>> = {
            let mapping = self.mapping.lock();
            SimpleDirOps::child_names(&*mapping)?
                .map(|name| Cow::Owned(name.into_owned()))
                .collect()
        };
        Ok(KBox::new(names.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        SimpleDirOps::lookup_child(&*self.mapping.lock(), lookup, name)
    }

    fn create_inode_child(
        &self,
        lookup: SimpleDirLookup<'_>,
        name: &str,
        inode: Arc<VfsInode>,
    ) -> VfsResult<Dentry> {
        SimpleDirOps::create_inode_child(&*self.mapping.lock(), lookup, name, inode)
    }

    fn create_file(
        &self,
        _dir: &VfsInode,
        _name: &str,
        mode: Umode,
        uid: u32,
        gid: u32,
    ) -> VfsResult<Arc<VfsInode>> {
        Ok(content::ContentFile::new_inode(
            self.fs.clone(),
            mode,
            uid,
            gid,
        ))
    }

    fn install_link(
        &self,
        lookup: SimpleDirLookup<'_>,
        target: &Dentry,
        name: &str,
    ) -> VfsResult<()> {
        SimpleDirOps::install_link(&*self.mapping.lock(), lookup, target, name)
    }

    fn mkdir(
        &self,
        _dir: &VfsInode,
        name: &str,
        mode: Umode,
        cred: &kcred::Cred,
    ) -> VfsResult<Arc<VfsInode>> {
        let inode = SimpleDir::new_inode_with_owner(
            self.fs.clone(),
            Arc::new(Self::new(self.fs.clone(), DirMapping::new())),
            mode.permission(),
            cred.fsuid(),
            cred.fsgid(),
        );
        let stored = inode.clone();
        self.mapping.lock().add_child(name, move |lookup, name| {
            Ok(lookup.dir_from_inode(name, stored.clone()))
        });
        Ok(inode)
    }

    fn unlink(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        SimpleDirOps::unlink(&*self.mapping.lock(), dir, dentry)
    }

    fn rmdir(&self, _dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        if self.mapping.lock().remove_child(dentry.name()) {
            Ok(())
        } else {
            Err(VfsError::NotFound)
        }
    }

    fn rename(
        &self,
        old: &LockedDentry<'_>,
        new: &LockedDentry<'_>,
        flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        // udev creates a device link under a temporary name and renames it into
        // place, so a rename inside one writable directory must work. Exchange,
        // whiteout and no-replace are not needed by that flow.
        if flags.intersects(
            kvfs::RenameFlags::NOREPLACE
                .union(kvfs::RenameFlags::EXCHANGE)
                .union(kvfs::RenameFlags::WHITEOUT),
        ) {
            return Err(VfsError::OperationNotSupported);
        }
        self.mapping.lock().rename_child(old.name(), new.name())
    }
}

pub(crate) struct DynamicDeviceDir {
    kind: DeviceNodeKind,
    writable: Arc<DirMapping>,
}

impl DynamicDeviceDir {
    pub(crate) fn input(fs: Arc<SimpleFs>) -> Self {
        let mut writable = DirMapping::new();
        for name in ["by-id", "by-path"] {
            writable.add_dir(
                name,
                SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
            );
        }
        Self {
            kind: DeviceNodeKind::Input,
            writable: Arc::new(writable),
        }
    }

    pub(crate) fn drm() -> Self {
        Self {
            kind: DeviceNodeKind::Drm,
            writable: Arc::new(DirMapping::new()),
        }
    }

    fn local_name<'a>(&self, node: &'a DeviceNode) -> Option<&'a str> {
        if node.kind() != self.kind {
            return None;
        }
        node.devname().rsplit('/').next()
    }

    /// Local name of the node's render minor (`renderD128`), when it has one.
    ///
    /// Derived from `DeviceNode::render_devname()` so the `128 + index` rule
    /// lives in the device contract, not in three projections.
    fn render_name(&self, node: &DeviceNode) -> Option<String> {
        // The precondition the callers rely on: only the `/dev/dri` directory
        // projects render minors. Without this check `/dev/input` would list
        // `renderD*` names for every DRM record that has a render node.
        if self.kind != DeviceNodeKind::Drm {
            return None;
        }
        node.render_devname()
            .and_then(|name| name.rsplit('/').next().map(String::from))
    }
}

impl SimpleDirOps for DynamicDeviceDir {
    fn child_names<'a>(&'a self) -> VfsResult<KBox<dyn Iterator<Item = Cow<'a, str>> + 'a>> {
        let mut names: Vec<Cow<'a, str>> = projection()
            .lock()
            .records
            .values()
            .flat_map(|record| {
                let primary = self
                    .local_name(&record.node)
                    .map(|name| Cow::Owned(String::from(name)));
                let render = record
                    .render_file
                    .as_ref()
                    .and_then(|_| self.render_name(&record.node))
                    .map(Cow::Owned);
                primary.into_iter().chain(render)
            })
            .collect();
        names.extend(self.writable.child_names()?);
        Ok(KBox::new(names.into_iter())?)
    }

    fn lookup_child(&self, lookup: SimpleDirLookup<'_>, name: &str) -> VfsResult<Dentry> {
        let file = projection().lock().records.values().find_map(|record| {
            if self.local_name(&record.node) == Some(name) {
                Some(record.file.clone())
            } else if record.render_file.is_some()
                && self.render_name(&record.node).as_deref() == Some(name)
            {
                record.render_file.clone()
            } else {
                None
            }
        });
        if let Some(file) = file {
            return Ok(device_dentry(lookup, name, file));
        }
        SimpleDirOps::lookup_child(self.writable.as_ref(), lookup, name)
    }

    fn create_inode_child(
        &self,
        lookup: SimpleDirLookup<'_>,
        name: &str,
        inode: Arc<kvfs::VfsInode>,
    ) -> VfsResult<Dentry> {
        self.writable.create_inode_child(lookup, name, inode)
    }

    fn unlink(&self, dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        SimpleDirOps::unlink(&*self.writable, dir, dentry)
    }

    fn rmdir(&self, _dir: &VfsInode, dentry: &LockedDentry<'_>) -> VfsResult<()> {
        if self.writable.remove_child(dentry.name()) {
            Ok(())
        } else {
            Err(VfsError::NotFound)
        }
    }

    fn rename(
        &self,
        old: &LockedDentry<'_>,
        new: &LockedDentry<'_>,
        _flags: kvfs::RenameFlags,
    ) -> VfsResult<()> {
        self.writable.rename_child(old.name(), new.name())
    }

    fn supports_dentry_cache(&self) -> bool {
        false
    }
}
