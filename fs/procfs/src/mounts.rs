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
use kfs::FS_CONTEXT;

struct ProcMountEntry {
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

fn mountpoint_path(mount: &Arc<Mountpoint>) -> String {
    mount
        .location()
        .and_then(|location| location.absolute_path().ok())
        .map(|path| path.to_string())
        .unwrap_or_else(|| "/".to_string())
}

fn mount_source(mount_point: &str, fs_type: &str) -> String {
    if mount_point == "/" {
        "rootfs".to_string()
    } else {
        fs_type.to_string()
    }
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

fn collect_mount_subtree(
    mount: &Arc<Mountpoint>,
    parent_id: u64,
    next_mount_id: &mut u64,
    mounts: &mut Vec<ProcMountEntry>,
) {
    let mount_id = *next_mount_id;
    *next_mount_id += 1;

    let mount_point = mountpoint_path(mount);
    let fs_type = mount.root_location().filesystem().name().to_string();
    let mount_flags = mount
        .root_location()
        .filesystem()
        .stat()
        .ok()
        .map_or(0, |stat| stat.mount_flags);
    let mount_options = format_mount_options(mount_flags);
    mounts.push(ProcMountEntry {
        mount_id,
        parent_id,
        major: 0,
        minor: mount.device() as u32,
        root: "/".to_string(),
        mount_point: mount_point.clone(),
        source: mount_source(&mount_point, &fs_type),
        fs_type,
        super_options: mount_options.clone(),
        mount_options,
    });

    let mut children = mount.child_mounts();
    children.sort_by_key(mountpoint_path);
    for child in children {
        collect_mount_subtree(&child, mount_id, next_mount_id, mounts);
    }
}

fn collect_proc_mounts() -> Vec<ProcMountEntry> {
    let root_mount = {
        let fs = FS_CONTEXT.lock();
        fs.root_dir().mountpoint().clone()
    };

    let mut mounts = Vec::new();
    let mut next_mount_id = 1;
    collect_mount_subtree(&root_mount, 0, &mut next_mount_id, &mut mounts);
    mounts
}

pub fn render_proc_mounts() -> String {
    collect_proc_mounts()
        .into_iter()
        .map(|entry| {
            format!(
                "{} {} {} {} 0 0\n",
                entry.source, entry.mount_point, entry.fs_type, entry.mount_options
            )
        })
        .collect()
}

pub fn render_mountinfo() -> String {
    collect_proc_mounts()
        .into_iter()
        .map(|entry| {
            format!(
                "{} {} {}:{} {} {} {} - {} {} {}\n",
                entry.mount_id,
                entry.parent_id,
                entry.major,
                entry.minor,
                entry.root,
                entry.mount_point,
                entry.mount_options,
                entry.fs_type,
                entry.source,
                entry.super_options,
            )
        })
        .collect()
}

pub fn render_mountstats() -> String {
    collect_proc_mounts()
        .into_iter()
        .map(|entry| {
            format!(
                "device {} mounted on {} with fstype {}\n",
                entry.source, entry.mount_point, entry.fs_type
            )
        })
        .collect()
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::{format_mount_options, mount_source};

    #[def_test]
    fn test_mount_source_for_root_uses_rootfs() {
        assert_eq!(mount_source("/", "proc"), "rootfs");
    }

    #[def_test]
    fn test_mount_source_for_non_root_uses_fs_type() {
        assert_eq!(mount_source("/proc", "proc"), "proc");
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
