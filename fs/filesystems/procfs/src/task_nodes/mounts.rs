// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use kvfs::{Mount, MountFlags, Path, SeqIterator, StatFsFlags};

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
    let st_flags = mount
        .filesystem_stat()
        .ok()
        .map_or(StatFsFlags::empty(), |stat| stat.mount_flags);
    let mount_ro = mnt_flags.contains(MountFlags::RDONLY);
    let super_ro = st_flags.contains(StatFsFlags::RDONLY);
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

#[cfg(unittest)]
mod tests {
    use alloc::{string::ToString, sync::Arc};

    use kvfs::{DirMapping, Mount, MountFlags, SimpleDir, SimpleFs};
    use unittest::{assert_eq, def_test};

    use super::ProcMountEntry;

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

    #[def_test]
    fn test_mount_source_uses_devname_when_set() {
        // Create a simple filesystem for testing.
        let fs = SimpleFs::new_with("test".into(), 0, |fs| {
            SimpleDir::<DirMapping>::new_maker(fs, Arc::new(DirMapping::new()))
        });

        // Create a root mount to act as the parent.
        let root_mount = Mount::new_root(&fs);
        let parent_path = root_mount.root_path();

        // Create a non-root child mount with a devname.
        let mount = Mount::new_with_flags_and_devname(
            &fs,
            Some(parent_path),
            MountFlags::empty(),
            Some("virtio-blk-0"),
        );

        let result = super::mount_source(&mount);
        assert_eq!(result, Some("virtio-blk-0".to_string()));
    }

    #[def_test]
    fn test_mount_source_falls_back_to_none_when_no_devname() {
        // Create a simple filesystem for testing.
        let fs = SimpleFs::new_with("test".into(), 0, |fs| {
            SimpleDir::<DirMapping>::new_maker(fs, Arc::new(DirMapping::new()))
        });

        // Create a root mount to act as the parent.
        let root_mount = Mount::new_root(&fs);
        let parent_path = root_mount.root_path();

        // Create a non-root child mount without a devname.
        let mount =
            Mount::new_with_flags_and_devname(&fs, Some(parent_path), MountFlags::empty(), None);

        let result = super::mount_source(&mount);
        assert_eq!(result, None);
    }
}
