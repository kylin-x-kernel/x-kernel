// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kvfs::{Mount, MountFlags, Path, SeqIterator, SuperBlockFlags};

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
    root: Option<Path>,
    stack: Vec<Arc<Mount>>,
}

pub(crate) struct ProcMountIter {
    collector: ProcMountCollector,
    formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result,
}

impl ProcMountCollector {
    fn new(root: Option<Path>) -> Self {
        Self {
            root,
            stack: Vec::new(),
        }
    }

    fn rewind(&mut self) {
        self.stack.clear();
        if let Some(root) = &self.root {
            self.stack.push(root.mount().clone());
        }
    }

    fn next_entry(&mut self) -> Option<ProcMountEntry> {
        let mount = self.stack.pop()?;
        let mount_id = mount.mount_id();
        let parent_id = mount
            .location()
            .map_or(0, |location| location.mount().mount_id());

        let children = sorted_mnt_mounts(&mount);
        for child in children.into_iter().rev() {
            self.stack.push(child);
        }

        Some(make_mount_entry(&mount, parent_id, mount_id))
    }
}

fn mangle(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            ' ' => result.push_str("\\040"),
            '\t' => result.push_str("\\011"),
            '\n' => result.push_str("\\012"),
            '\\' => result.push_str("\\\\"),
            '#' => result.push_str("\\043"),
            _ => result.push(c),
        }
    }
    result
}

fn show_mounts(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    let mount_options = format_mount_options(item.mnt_flags, item.mount_ro || item.super_ro);
    buf.push_str(&format!(
        "{} {} {} {} 0 0\n",
        mangle(&item.source),
        item.mount_point,
        item.fs_type,
        mount_options
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
        mangle(&item.source),
        super_options,
    ));
    Ok(())
}

fn show_mountstats(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    buf.push_str(&format!(
        "device {} mounted on {} with fstype {}\n",
        mangle(&item.source),
        item.mount_point,
        item.fs_type
    ));
    Ok(())
}

impl ProcMountIter {
    pub(crate) fn mounts(root: Option<Path>) -> Self {
        Self::new(root, show_mounts)
    }

    pub(crate) fn mountinfo(root: Option<Path>) -> Self {
        Self::new(root, show_mountinfo)
    }

    pub(crate) fn mountstats(root: Option<Path>) -> Self {
        Self::new(root, show_mountstats)
    }

    fn new(
        root: Option<Path>,
        formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result,
    ) -> Self {
        Self {
            collector: ProcMountCollector::new(root),
            formatter,
        }
    }
}

fn mountpoint_path(mount: &Arc<Mount>) -> String {
    match mount.location() {
        None => "/".to_string(),
        Some(location) => location
            .absolute_path()
            .map(|path| path.to_string())
            .unwrap_or_else(|_| format!("<mount:{}>", location.name())),
    }
}

fn mount_source(mount: &Arc<Mount>) -> Option<String> {
    if let Some(devname) = mount.devname() {
        return Some(devname.to_string());
    }
    None
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

fn make_mount_entry(mount: &Arc<Mount>, parent_id: u64, mount_id: u64) -> ProcMountEntry {
    let mount_point = mountpoint_path(mount);
    let fs_type = mount.filesystem_name().to_string();
    let mnt_flags = mount.flags();
    let mount_ro = mnt_flags.contains(MountFlags::RDONLY);
    let super_ro = mount.super_block_flags().contains(SuperBlockFlags::RDONLY);
    ProcMountEntry {
        mount_id,
        parent_id,
        major: 0,
        minor: mount.synthetic_device_id() as u32,
        root: "/".to_string(),
        mount_point: mount_point.clone(),
        source: mount_source(mount).unwrap_or_else(|| "none".to_string()),
        fs_type,
        mnt_flags,
        mount_ro,
        super_ro,
    }
}

fn sorted_mnt_mounts(mount: &Arc<Mount>) -> Vec<Arc<Mount>> {
    let mut children = mount.children();
    children.sort_by_key(mountpoint_path);
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

unittest::path_mod_test!(tests, "../../ut-tests/task_nodes_mounts.rs");
