// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use fs_ng_vfs::{Mountpoint, ST_NOATIME, ST_NODEV, ST_NOEXEC, ST_NOSUID, ST_RDONLY, ST_RELATIME};
use kcore::vfs::SeqIterator;
use kfs::FS_CONTEXT;

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
    mount_options: String,
    super_options: String,
}

struct ProcMountCollector {
    stack: Vec<(Arc<Mountpoint>, u64)>,
    next_mount_id: u64,
}

pub(crate) struct ProcMountIter {
    collector: ProcMountCollector,
    formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result,
}

impl ProcMountCollector {
    fn new() -> Self {
        Self {
            stack: Vec::new(),
            next_mount_id: 1,
        }
    }

    fn rewind(&mut self) {
        self.stack.clear();
        self.next_mount_id = 1;
        self.stack.push((root_mountpoint(), 0));
    }

    fn next_entry(&mut self) -> Option<ProcMountEntry> {
        let (mount, parent_id) = self.stack.pop()?;
        let mount_id = self.next_mount_id;
        self.next_mount_id += 1;

        let children = sorted_child_mounts(&mount);
        for child in children.into_iter().rev() {
            self.stack.push((child, mount_id));
        }

        Some(make_mount_entry(&mount, parent_id, mount_id))
    }
}

fn show_mounts(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    buf.push_str(&format!(
        "{} {} {} {} 0 0\n",
        item.source, item.mount_point, item.fs_type, item.mount_options
    ));
    Ok(())
}

fn show_mountinfo(item: &ProcMountEntry, buf: &mut String) -> core::fmt::Result {
    buf.push_str(&format!(
        "{} {} {}:{} {} {} {} - {} {} {}\n",
        item.mount_id,
        item.parent_id,
        item.major,
        item.minor,
        item.root,
        item.mount_point,
        item.mount_options,
        item.fs_type,
        item.source,
        item.super_options,
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
        Self::new(show_mounts)
    }

    pub(crate) fn mountinfo() -> Self {
        Self::new(show_mountinfo)
    }

    pub(crate) fn mountstats() -> Self {
        Self::new(show_mountstats)
    }

    fn new(formatter: fn(&ProcMountEntry, &mut String) -> core::fmt::Result) -> Self {
        Self {
            collector: ProcMountCollector::new(),
            formatter,
        }
    }
}

fn mountpoint_path(mount: &Arc<Mountpoint>) -> String {
    match mount.location() {
        None => "/".to_string(),
        Some(location) => location
            .absolute_path()
            .map(|path| path.to_string())
            .unwrap_or_else(|_| format!("<mount:{}>", location.name())),
    }
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

fn format_mount_options(mount_flags: u32) -> String {
    let mut options = Vec::new();
    options.push(if mount_flags & ST_RDONLY != 0 {
        "ro"
    } else {
        "rw"
    });
    push_mount_option(&mut options, mount_flags & ST_NOSUID != 0, "nosuid");
    push_mount_option(&mut options, mount_flags & ST_NODEV != 0, "nodev");
    push_mount_option(&mut options, mount_flags & ST_NOEXEC != 0, "noexec");
    push_mount_option(&mut options, mount_flags & ST_NOATIME != 0, "noatime");
    push_mount_option(&mut options, mount_flags & ST_RELATIME != 0, "relatime");
    options.join(",")
}

fn make_mount_entry(mount: &Arc<Mountpoint>, parent_id: u64, mount_id: u64) -> ProcMountEntry {
    let mount_point = mountpoint_path(mount);
    let fs_type = mount.root_location().filesystem().name().to_string();
    let mount_flags = mount
        .root_location()
        .filesystem()
        .stat()
        .ok()
        .map_or(0, |stat| stat.mount_flags);
    let mount_options = format_mount_options(mount_flags);
    ProcMountEntry {
        mount_id,
        parent_id,
        major: 0,
        minor: mount.device() as u32,
        root: "/".to_string(),
        mount_point: mount_point.clone(),
        source: mount_source(mount, &fs_type),
        fs_type,
        super_options: mount_options.clone(),
        mount_options,
    }
}

fn root_mountpoint() -> Arc<Mountpoint> {
    let fs = FS_CONTEXT.lock();
    fs.root_dir().mountpoint().clone()
}

fn sorted_child_mounts(mount: &Arc<Mountpoint>) -> Vec<Arc<Mountpoint>> {
    let mut children = mount.child_mounts();
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

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{format_mount_options, mount_source_for};

    #[def_test]
    fn test_mount_source_for_root_uses_rootfs() {
        assert_eq!(mount_source_for(true, "proc"), "rootfs");
    }

    #[def_test]
    fn test_mount_source_for_non_root_uses_fs_type() {
        assert_eq!(mount_source_for(false, "proc"), "proc");
    }

    #[def_test]
    fn test_format_mount_options_defaults_to_rw() {
        assert_eq!(format_mount_options(0), "rw");
    }

    #[def_test]
    fn test_format_mount_options_includes_all_enabled_flags() {
        let flags = 0x1 | 0x2 | 0x4 | 0x8 | 0x400 | 0x1000;
        assert_eq!(
            format_mount_options(flags),
            "ro,nosuid,nodev,noexec,noatime,relatime"
        );
    }
}
