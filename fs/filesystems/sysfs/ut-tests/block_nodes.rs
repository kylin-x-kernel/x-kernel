// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn stat_has_linux_field_order_sector_units_and_millisecond_conversion() {
    let stats = block::BlockIoStats {
        read_ios: 2,
        read_sectors: 16,
        read_time_ns: 3_900_000,
        write_ios: 4,
        write_sectors: 32,
        write_time_ns: 5_100_000,
        in_flight: 6,
        io_time_ns: 7_900_000,
        weighted_time_ns: 8_100_000,
        flush_ios: 9,
        flush_time_ns: 10_900_000,
    };
    assert_eq!(
        render_stat(stats),
        b"2 0 16 3 4 0 32 5 6 7 8 0 0 0 0 9 10\n"
    );
}

struct Backend;

impl block::BlockDeviceOperations for Backend {
    fn num_blocks(&self) -> u64 {
        8
    }

    fn block_size(&self) -> usize {
        4096
    }
}

impl block::BlkMqOperations for Backend {
    fn queue_rq(&self, request: block::Request) {
        request.complete(Err(block::DriverError::Io));
    }
}

struct PublishedDisk(block::GendiskRef);

impl Drop for PublishedDisk {
    fn drop(&mut self) {
        if block::lookup_block_device(self.0.device_number())
            .is_some_and(|current| Arc::ptr_eq(&current, &self.0.part0()))
        {
            block::del_gendisk(self.0.device_number());
        }
    }
}

#[def_test(serial)]
fn block_projection_tracks_publication_live_state_removal_and_number_reuse() {
    init_device_projection();
    let disk = || {
        block::Gendisk::alloc_mq(
            String::from("sysfs-block-regression"),
            245,
            910_000,
            1,
            Arc::new(Backend),
        )
        .unwrap()
    };
    let first = PublishedDisk(disk());
    let device = block::add_disk(first.0.clone()).unwrap();
    let path = "block/sysfs-block-regression/stat";
    let old_entry = device_projection().lock().tree.get(path).cloned().unwrap();
    assert!(matches!(
        old_entry,
        TreeEntry::BlockAttribute(_, Attribute::Stat)
    ));
    assert_eq!(
        read_attribute(&device, Attribute::Dev).unwrap(),
        b"245:910000\n"
    );
    assert_eq!(read_attribute(&device, Attribute::Size).unwrap(), b"64\n");
    device.set_capacity(32).unwrap();
    device.set_disk_read_only(true).unwrap();
    assert_eq!(read_attribute(&device, Attribute::Size).unwrap(), b"32\n");
    assert_eq!(
        read_attribute(&device, Attribute::ReadOnly).unwrap(),
        b"1\n"
    );
    let tree = device_projection().lock().tree.clone();
    for link in ["class/block/sysfs-block-regression", "dev/block/245:910000"] {
        let TreeEntry::Symlink(target) = tree.get(link).unwrap() else {
            panic!("block alias must be a symlink")
        };
        assert_eq!(target, "../../block/sysfs-block-regression");
    }
    drop(tree);
    block::del_gendisk(device.device_number()).unwrap();
    assert!(!device_projection().lock().tree.contains_key(path));
    assert_eq!(
        read_attribute(&device, Attribute::Stat),
        Err(VfsError::NoSuchDevice)
    );

    let replacement = PublishedDisk(disk());
    let new_device = block::add_disk(replacement.0.clone()).unwrap();
    assert_eq!(
        read_attribute(&new_device, Attribute::Size).unwrap(),
        b"64\n"
    );
    assert_eq!(
        read_attribute(&device, Attribute::Stat),
        Err(VfsError::NoSuchDevice)
    );
    assert!(device_projection().lock().tree.contains_key(path));
}
