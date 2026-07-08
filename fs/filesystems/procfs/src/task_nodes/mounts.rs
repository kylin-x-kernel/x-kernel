// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kprocess::{AsThread, current_user_process_fs_context};
use ktask::WeakKtaskRef;
use kvfs::{Location, MountFlags, Mountpoint, ST_RDONLY};
use kvfs_simple::{DirMapping, SeqFileNode, SeqIterator, SimpleFs};

#[derive(Clone)]
pub(crate) struct ProcMountEntry {
    mount_id: u64,
    parent_id: u64,
    major: u32,
    minor: u32,
    root: String,
    mount_point: String,
    fs_type: String,
    source: String,
    mnt_flags: MountFlags,
    mount_ro: bool,
    super_ro: bool,
}

struct ProcMountCollector {
    source: ProcMountSource,
    root: Option<Location>,
    stack: Vec<(Arc<Mountpoint>, u64)>,
    next_mount_id: u64,
}

#[derive(Clone)]
enum ProcMountSource {
    Current,
    Task(WeakKtaskRef),
}

pub(crate) struct ProcMountIter {
    collector: ProcMountCollector,
    formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result,
}

impl ProcMountCollector {
    fn new(source: ProcMountSource) -> Self {
        Self {
            source,
            root: None,
            stack: Vec::new(),
            next_mount_id: 1,
        }
    }

    fn rewind(&mut self) {
        self.stack.clear();
        self.next_mount_id = 1;
        self.root = root_location_for_source(&self.source);
        if let Some(root) = &self.root {
            self.stack.push((root.mountpoint().clone(), 0));
        }
    }

    fn next_entry(&mut self) -> Option<ProcMountEntry> {
        let root = self.root.as_ref()?;
        while let Some((mount, parent_id)) = self.stack.pop() {
            let mount_id = self.next_mount_id;
            self.next_mount_id += 1;

            let children = sorted_child_mounts(&mount, root);
            for child in children.into_iter().rev() {
                self.stack.push((child, mount_id));
            }

            if let Some(entry) = make_mount_entry(&mount, root, parent_id, mount_id) {
                return Some(entry);
            }
        }
        None
    }
}

impl ProcMountSource {
    fn root_location(&self) -> Option<Location> {
        match self {
            Self::Current => {
                let fs_context = current_user_process_fs_context();
                let fs = fs_context.lock();
                Some(fs.root_dir().clone())
            }
            Self::Task(task) => {
                let task = task.upgrade()?;
                let fs_context = task.as_thread().process().fs_context().ok()?;
                let fs = fs_context.lock();
                Some(fs.root_dir().clone())
            }
        }
    }
}

fn show_mounts(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    let mount_options = format_mount_options(item.mnt_flags, item.mount_ro || item.super_ro);
    buf.push_str(&format!(
        "{} {} {} {} 0 0\n",
        item.source, item.mount_point, item.fs_type, mount_options
    ));
    Ok(())
}

fn show_mountinfo(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    let mount_options = format_mount_options(item.mnt_flags, item.mount_ro);
    let super_options = if item.super_ro { "ro" } else { "rw" };
    buf.push_str(&format!(
        "{} {} {}:{} {} {} {} - {} {} {}\n",
        item.mount_id,
        item.parent_id,
        item.major,
        item.minor,
        item.root,
        item.mount_point,
        mount_options,
        item.fs_type,
        item.source,
        super_options,
    ));
    Ok(())
}

fn show_mountstats(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    buf.push_str(&format!(
        "device {} mounted on {} with fstype {}\n",
        item.source, item.mount_point, item.fs_type
    ));
    Ok(())
}

impl ProcMountIter {
    pub(crate) fn mounts() -> Self {
        Self::new(ProcMountSource::Current, show_mounts)
    }

    pub(crate) fn mounts_for_task(task: WeakKtaskRef) -> Self {
        Self::new(ProcMountSource::Task(task), show_mounts)
    }

    pub(crate) fn mountinfo_for_task(task: WeakKtaskRef) -> Self {
        Self::new(ProcMountSource::Task(task), show_mountinfo)
    }

    pub(crate) fn mountstats_for_task(task: WeakKtaskRef) -> Self {
        Self::new(ProcMountSource::Task(task), show_mountstats)
    }

    fn new(
        source: ProcMountSource,
        formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result,
    ) -> Self {
        Self {
            collector: ProcMountCollector::new(source),
            formatter,
        }
    }
}

fn mountpoint_path(mount: &Arc<Mountpoint>, root: &Location) -> Option<String> {
    if Arc::ptr_eq(root.mountpoint(), mount) {
        return Some("/".to_string());
    }

    let location = mount.location()?;
    location
        .path_from_root(root)
        .ok()
        .map(|path| path.to_string())
}

fn mount_source_for(is_root: bool, fs_type: &str) -> String {
    if is_root {
        "rootfs".to_string()
    } else {
        fs_type.to_string()
    }
}

fn mount_source(mount: &Arc<Mountpoint>, fs_type: &str) -> String {
    mount_source_for(mount.is_root(), fs_type)
}

fn push_mount_option(options: &mut Vec<&'static str>, enabled: bool, name: &'static str) {
    if enabled {
        options.push(name);
    }
}

/// Build the mount-option string from per-mount flags and the supplied readonly state.
fn format_mount_options(mnt_flags: MountFlags, is_ro: bool) -> String {
    let mut options = Vec::with_capacity(8);
    options.push(if is_ro { "ro" } else { "rw" });
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::NOSUID),
        "nosuid",
    );
    push_mount_option(&mut options, mnt_flags.contains(MountFlags::NODEV), "nodev");
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::NOEXEC),
        "noexec",
    );
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::NOATIME),
        "noatime",
    );
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::NODIRATIME),
        "nodiratime",
    );
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::RELATIME),
        "relatime",
    );
    push_mount_option(
        &mut options,
        mnt_flags.contains(MountFlags::NOSYMFOLLOW),
        "nosymfollow",
    );
    options.join(",")
}

fn make_mount_entry(
    mount: &Arc<Mountpoint>,
    root: &Location,
    parent_id: u64,
    mount_id: u64,
) -> Option<ProcMountEntry> {
    let mount_point = mountpoint_path(mount, root)?;
    let fs_type = mount.super_block().name().to_string();
    let mnt_flags = mount.flags();
    let st_flags = mount
        .super_block()
        .stat()
        .ok()
        .map_or(0, |stat| stat.mount_flags);
    let mount_ro = mnt_flags.contains(MountFlags::RDONLY);
    let super_ro = st_flags & ST_RDONLY != 0;
    Some(ProcMountEntry {
        mount_id,
        parent_id,
        major: 0,
        minor: mount.device() as u32,
        root: "/".to_string(),
        mount_point: mount_point.clone(),
        source: mount_source(mount, &fs_type),
        fs_type,
        mnt_flags,
        mount_ro,
        super_ro,
    })
}

fn root_location_for_source(source: &ProcMountSource) -> Option<Location> {
    source.root_location()
}

fn sorted_child_mounts(mount: &Arc<Mountpoint>, root: &Location) -> Vec<Arc<Mountpoint>> {
    let mut children = mount.child_mounts();
    children.sort_by_key(|child| {
        mountpoint_path(child, root).unwrap_or_else(|| format!("~mount:{}", child.device()))
    });
    children
}

impl SeqIterator for ProcMountIter {
    type Item = ProcMountEntry;

    fn rewind(&mut self) {
        self.collector.rewind();
    }

    fn start(&mut self) -> Option<Self::Item> {
        self.rewind();
        self.next()
    }

    fn next(&mut self) -> Option<Self::Item> {
        self.collector.next_entry()
    }

    fn show(&self, item: &Self::Item, buf: &mut String) -> core::fmt::Result {
        (self.formatter)(item, buf)
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "mounts",
        SeqFileNode::new_regular(fs, ProcMountIter::mounts()),
    );
}

#[cfg(unittest)]
mod tests {
    use kvfs::MountFlags;
    use unittest::{assert_eq, def_test};

    use super::ProcMountEntry;

    #[def_test]
    fn test_mount_source_for_root_uses_rootfs() {
        assert_eq!(super::mount_source_for(true, "proc"), "rootfs");
    }

    #[def_test]
    fn test_mount_source_for_non_root_uses_fs_type() {
        assert_eq!(super::mount_source_for(false, "proc"), "proc");
    }

    #[def_test]
    fn test_format_mount_options_defaults_to_rw() {
        assert_eq!(
            super::format_mount_options(MountFlags::empty(), false),
            "rw"
        );
    }

    #[def_test]
    fn test_format_ro_uses_supplied_readonly_state() {
        assert_eq!(super::format_mount_options(MountFlags::RDONLY, true), "ro");
        assert_eq!(super::format_mount_options(MountFlags::empty(), true), "ro");
        assert_eq!(super::format_mount_options(MountFlags::RDONLY, false), "rw");
        assert_eq!(
            super::format_mount_options(MountFlags::empty(), false),
            "rw"
        );
    }

    #[def_test]
    fn test_format_mount_options_shows_per_mount_flags() {
        let mnt = MountFlags::NOSUID
            | MountFlags::NODEV
            | MountFlags::NOEXEC
            | MountFlags::NOATIME
            | MountFlags::RELATIME;
        let opts = super::format_mount_options(mnt, false);
        assert!(opts.contains("nosuid"));
        assert!(opts.contains("nodev"));
        assert!(opts.contains("noexec"));
        assert!(opts.contains("noatime"));
        assert!(opts.contains("relatime"));
        assert!(!opts.contains("nodiratime"));
        assert!(!opts.contains("nosymfollow"));
    }

    #[def_test]
    fn test_format_mount_options_includes_nodiratime_and_nosymfollow() {
        let mnt = MountFlags::NODIRATIME | MountFlags::NOSYMFOLLOW;
        let opts = super::format_mount_options(mnt, false);
        assert!(opts.contains("nodiratime"));
        assert!(opts.contains("nosymfollow"));
    }

    #[def_test]
    fn test_mounts_uses_effective_readonly_options() {
        let entry = ProcMountEntry {
            mount_id: 1,
            parent_id: 0,
            major: 0,
            minor: 1,
            root: "/".into(),
            mount_point: "/".into(),
            fs_type: "ext4".into(),
            source: "rootfs".into(),
            mnt_flags: MountFlags::empty(),
            mount_ro: false,
            super_ro: true,
        };
        let mut buf = alloc::string::String::new();

        super::show_mounts(&entry, &mut buf).unwrap();

        assert_eq!(buf, "rootfs / ext4 ro 0 0\n");
    }

    #[def_test]
    fn test_mountinfo_separates_mount_and_super_readonly() {
        let entry = ProcMountEntry {
            mount_id: 1,
            parent_id: 0,
            major: 0,
            minor: 1,
            root: "/".into(),
            mount_point: "/".into(),
            fs_type: "ext4".into(),
            source: "rootfs".into(),
            mnt_flags: MountFlags::empty(),
            mount_ro: false,
            super_ro: true,
        };
        let mut buf = alloc::string::String::new();

        super::show_mountinfo(&entry, &mut buf).unwrap();

        assert_eq!(buf, "1 0 0:1 / / rw - ext4 rootfs ro\n");
    }
}
