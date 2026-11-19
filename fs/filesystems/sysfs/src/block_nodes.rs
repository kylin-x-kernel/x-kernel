// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Read-only block inventory and live Linux block statistics.

use ktime_types::NANOS_PER_MILLIS;

use super::*;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Attribute {
    Stat,
    Dev,
    Size,
    ReadOnly,
}

pub(super) fn add_entries(
    tree: &mut BTreeMap<String, TreeEntry>,
    devices: &[Arc<block::BlockDevice>],
) {
    for device in devices {
        let leaf = format!("block/{}", device.disk().name());
        for (name, attribute) in [
            ("stat", Attribute::Stat),
            ("dev", Attribute::Dev),
            ("size", Attribute::Size),
            ("ro", Attribute::ReadOnly),
        ] {
            add_entry(
                tree,
                format!("{leaf}/{name}"),
                TreeEntry::BlockAttribute(device.clone(), attribute),
            );
        }
        let number = device.device_number();
        for link in [
            format!("class/block/{}", device.disk().name()),
            format!("dev/block/{}:{}", number.major(), number.minor()),
        ] {
            add_entry(
                tree,
                link.clone(),
                TreeEntry::Symlink(relative_symlink(&link, &leaf)),
            );
        }
    }
}

pub(super) fn attribute_file(
    fs: Arc<SimpleFs>,
    device: Arc<block::BlockDevice>,
    attribute: Attribute,
) -> Arc<SimpleFile> {
    SimpleFile::new_regular_with_permission(fs, READ_ONLY_PERMISSION, move || {
        read_attribute(&device, attribute)
    })
}

fn read_attribute(device: &Arc<block::BlockDevice>, attribute: Attribute) -> VfsResult<Vec<u8>> {
    let is_registered = block::lookup_block_device(device.device_number())
        .is_some_and(|current| Arc::ptr_eq(&current, device));
    if !is_registered {
        return Err(VfsError::NoSuchDevice);
    }
    Ok(match attribute {
        Attribute::Stat => render_stat(device.io_stats()),
        Attribute::Dev => {
            let number = device.device_number();
            append_line(&format!("{}:{}", number.major(), number.minor()))
        }
        Attribute::Size => append_line(&device.num_sectors().to_string()),
        Attribute::ReadOnly => append_line(if device.is_read_only() { "1" } else { "0" }),
    })
}

fn render_stat(stats: block::BlockIoStats) -> Vec<u8> {
    // Linux block/stat.rst: 17 fields, 512-byte sectors and milliseconds.
    // There is no request merging or discard operation in the current block
    // core, so their counters are zero. Explicit flushes have their own fields.
    format!(
        "{} 0 {} {} {} 0 {} {} {} {} {} 0 0 0 0 {} {}\n",
        stats.read_ios,
        stats.read_sectors,
        stats.read_time_ns / NANOS_PER_MILLIS,
        stats.write_ios,
        stats.write_sectors,
        stats.write_time_ns / NANOS_PER_MILLIS,
        stats.in_flight,
        stats.io_time_ns / NANOS_PER_MILLIS,
        stats.weighted_time_ns / NANOS_PER_MILLIS,
        stats.flush_ios,
        stats.flush_time_ns / NANOS_PER_MILLIS,
    )
    .into_bytes()
}

unittest::path_mod_test!(tests, "../ut-tests/block_nodes.rs");
