// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};

use ksync::Mutex;
use unittest::def_test;

use super::*;

const BLOCK_SIZE: usize = 512;

struct MemoryDisk {
    block_size: usize,
    storage: Mutex<Vec<u8>>,
}

struct GeometryDisk {
    blocks: u64,
    block_size: usize,
}

struct InherentlyReadOnlyDisk(MemoryDisk);

impl MemoryDisk {
    fn new(blocks: u64) -> Self {
        Self::with_block_size(blocks, BLOCK_SIZE)
    }

    fn with_block_size(blocks: u64, block_size: usize) -> Self {
        let blocks = usize::try_from(blocks).expect("test block count must fit in usize");
        let len = blocks
            .checked_mul(block_size)
            .expect("test disk size must fit in usize");
        Self {
            block_size,
            storage: Mutex::new(vec![0; len]),
        }
    }
}

impl BlockDeviceOperations for MemoryDisk {
    fn num_blocks(&self) -> u64 {
        (self.storage.lock().len() / self.block_size) as u64
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        let block_id = usize::try_from(block_id).map_err(|_| DriverError::InvalidInput)?;
        let start = block_id
            .checked_mul(self.block_size)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::InvalidInput)?;
        let storage = self.storage.lock();
        let source = storage.get(start..end).ok_or(DriverError::InvalidInput)?;
        buf.copy_from_slice(source);
        Ok(())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        let block_id = usize::try_from(block_id).map_err(|_| DriverError::InvalidInput)?;
        let start = block_id
            .checked_mul(self.block_size)
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(buf.len())
            .ok_or(DriverError::InvalidInput)?;
        let mut storage = self.storage.lock();
        let target = storage
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?;
        target.copy_from_slice(buf);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl BlockDeviceOperations for GeometryDisk {
    fn num_blocks(&self) -> u64 {
        self.blocks
    }

    fn block_size(&self) -> usize {
        self.block_size
    }

    fn read_block(&self, _block_id: u64, _buf: &mut [u8]) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    fn write_block(&self, _block_id: u64, _buf: &[u8]) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

impl BlockDeviceOperations for InherentlyReadOnlyDisk {
    fn num_blocks(&self) -> u64 {
        self.0.num_blocks()
    }

    fn block_size(&self) -> usize {
        self.0.block_size()
    }

    fn is_inherently_read_only(&self) -> bool {
        true
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.0.read_block(block_id, buf)
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> DriverResult {
        self.0.write_block(block_id, buf)
    }

    fn flush(&self) -> DriverResult {
        self.0.flush()
    }
}

fn disk(name: &str, major: u32, first_minor: u32, minors: u32) -> GendiskRef {
    Gendisk::alloc(
        String::from(name),
        major,
        first_minor,
        minors,
        Box::new(MemoryDisk::new(8)),
    )
    .expect("valid test disk")
}

fn ioctl_bytes(device: &BlockDevice, cmd: u32) -> Vec<u8> {
    let mut output = Vec::new();
    device
        .ioctl(
            BlockOpenMode::READ,
            cmd,
            1,
            false,
            || panic!("query must not read the argument"),
            |bytes| {
                output.extend_from_slice(bytes);
                Ok(())
            },
        )
        .expect("generic query succeeds without driver ioctl support");
    output
}

#[def_test]
fn generic_ioctls_report_queue_geometry_and_sector_capacity() {
    use linux_raw_sys::ioctl::{
        BLKALIGNOFF, BLKGETDISKSEQ, BLKGETSIZE, BLKGETSIZE64, BLKIOMIN, BLKIOOPT, BLKPBSZGET,
        BLKROGET, BLKSSZGET,
    };

    let disk = Gendisk::alloc(
        String::from("ioctl-4k"),
        249,
        0,
        1,
        Box::new(GeometryDisk {
            blocks: 2,
            block_size: 4096,
        }),
    )
    .unwrap();
    let device = disk.part0();
    for (cmd, expected) in [
        (BLKSSZGET, 4096_u32),
        (BLKPBSZGET, 4096),
        (BLKIOMIN, 4096),
        (BLKIOOPT, 0),
        (BLKALIGNOFF, 0),
        (BLKROGET, 0),
    ] {
        assert_eq!(ioctl_bytes(&device, cmd), expected.to_ne_bytes());
    }
    device.set_capacity(9).unwrap();
    assert_eq!(ioctl_bytes(&device, BLKGETSIZE), 9_usize.to_ne_bytes());
    assert_eq!(ioctl_bytes(&device, BLKGETSIZE64), 4608_u64.to_ne_bytes());
    assert_eq!(
        ioctl_bytes(&device, BLKGETDISKSEQ),
        disk.diskseq().to_ne_bytes()
    );
}

#[def_test]
fn readahead_ioctls_share_disk_policy_and_linux_argument_semantics() {
    use linux_raw_sys::ioctl::{BLKFRAGET, BLKFRASET, BLKRAGET, BLKRASET};

    let disk = disk("ioctl-readahead", 249, 0, 1);
    let device = disk.part0();
    let set = |cmd, sectors, has_sys_admin| {
        device.as_ref().ioctl(
            BlockOpenMode::READ,
            cmd,
            sectors,
            has_sys_admin,
            || panic!("readahead setter takes a value, not a pointer"),
            |_| panic!("setter has no output"),
        )
    };
    for cmd in [BLKRAGET, BLKFRAGET] {
        assert_eq!(ioctl_bytes(&device, cmd), 256_usize.to_ne_bytes());
        assert_eq!(
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                cmd,
                0,
                false,
                || panic!("getter has no input"),
                |_| panic!("null is rejected before copying"),
            ),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            device.as_ref().ioctl(
                BlockOpenMode::READ,
                cmd,
                1,
                false,
                || panic!("getter has no input"),
                |_| Err(KError::BadAddress),
            ),
            Err(KError::BadAddress)
        );
    }
    for cmd in [BLKRASET, BLKFRASET] {
        assert_eq!(set(cmd, 9, false), Err(KError::PermissionDenied));
        assert_eq!(set(cmd, 9, true), Ok(0));
        assert_eq!(disk.backing_dev_info().readahead_pages(), 1);
        for getter in [BLKRAGET, BLKFRAGET] {
            assert_eq!(ioctl_bytes(&device, getter), 8_usize.to_ne_bytes());
        }
        assert_eq!(set(cmd, 0, true), Ok(0));
        assert_eq!(disk.backing_dev_info().readahead_pages(), 0);
    }
}

#[def_test(serial)]
fn disk_sequence_tracks_media_not_publication_or_capacity() {
    let first = disk("sequence-first", 249, 0, 1);
    let sequence = first.diskseq();
    assert_ne!(sequence, 0);
    let device = add_disk(first.clone()).unwrap();
    assert_eq!(device.disk().diskseq(), sequence);
    device.set_capacity(3).unwrap();
    del_gendisk(first.device_number()).unwrap();
    add_disk(first.clone()).unwrap();
    assert_eq!(first.diskseq(), sequence);
    del_gendisk(first.device_number()).unwrap();
    let second = disk("sequence-reused-devt", 249, 0, 1);
    assert!(second.diskseq() > sequence);
    first.inc_diskseq();
    assert!(device.disk().diskseq() > second.diskseq());
    assert_eq!(device.num_sectors(), 3);
}

#[def_test]
fn unpublished_part0_keeps_its_complete_disk_alive() {
    let backend = Arc::new(MemoryDisk::new(8));
    let backend_weak = Arc::downgrade(&backend);
    let disk = Gendisk::alloc(
        String::from("test-unpublished"),
        239,
        0,
        1,
        Box::new(backend),
    )
    .expect("valid unpublished disk");
    let part0 = disk.part0();
    let disk_weak = Arc::downgrade(&part0.disk);
    let part0_weak = Arc::downgrade(&part0);
    let queue_weak = Arc::downgrade(disk.queue());
    let disk_identity = part0.disk() as *const Gendisk;

    assert_eq!(part0.device_number(), DeviceNumber::new(239, 0));
    assert_eq!(part0.start_sector(), 0);
    assert_eq!(part0.num_sectors(), 8);
    assert!(Arc::ptr_eq(disk.queue(), part0.queue()));
    assert!(core::ptr::eq(part0.disk(), &*disk));
    assert!(lookup_block_device(part0.device_number()).is_none());
    drop(disk);

    assert!(core::ptr::eq(part0.disk(), disk_identity));
    assert_eq!(part0.disk().name(), "test-unpublished");
    assert!(Arc::ptr_eq(&part0.disk().part0(), &part0));
    part0
        .write_block(0, &[0x5a; BLOCK_SIZE])
        .expect("write unpublished disk");
    let mut data = [0; BLOCK_SIZE];
    part0
        .read_block(0, &mut data)
        .expect("read unpublished disk");
    assert_eq!(data, [0x5a; BLOCK_SIZE]);
    assert!(backend_weak.upgrade().is_some());
    drop(part0);

    assert!(part0_weak.upgrade().is_none());
    assert!(disk_weak.upgrade().is_none());
    assert!(queue_weak.upgrade().is_none());
    assert!(backend_weak.upgrade().is_none());
}

#[def_test]
fn cloned_disk_reference_keeps_canonical_part0_alive() {
    let disk = disk("test-disk-reference", 249, 0, 1);
    let part0 = disk.part0();
    let part0_weak = Arc::downgrade(&part0);
    let disk_weak = Arc::downgrade(&part0.disk);
    let retained = disk.clone();
    drop(part0);
    drop(disk);

    assert!(Arc::ptr_eq(
        &retained.part0(),
        &part0_weak.upgrade().expect("retained part0"),
    ));
    assert!(core::ptr::eq(retained.part0().disk(), &*retained));
    drop(retained);
    assert!(part0_weak.upgrade().is_none());
    assert!(disk_weak.upgrade().is_none());
}

#[def_test(serial)]
fn add_disk_publishes_canonical_part0_and_checks_minor_ranges() {
    let first = disk("test-range-a", 240, 64, 16);
    let part0 = add_disk(first.clone()).expect("publish first disk");
    assert!(Arc::ptr_eq(
        &part0,
        &lookup_block_device(DeviceNumber::new(240, 64)).expect("lookup part0")
    ));
    assert!(Arc::ptr_eq(&part0, &first.part0()));

    let overlap = disk("test-range-overlap", 240, 70, 1);
    assert!(matches!(add_disk(overlap), Err(DriverError::AlreadyExists)));

    let adjacent = disk("test-range-b", 240, 80, 1);
    add_disk(adjacent.clone()).expect("adjacent range is valid");
    del_gendisk(adjacent.device_number()).expect("remove adjacent disk");
    del_gendisk(first.device_number()).expect("remove first disk");
}

#[def_test(serial)]
fn block_device_validates_the_complete_io_extent() {
    let disk = disk("test-bounds", 241, 0, 1);
    let device = add_disk(disk.clone()).expect("publish bounds disk");
    let mut two_blocks = vec![0; BLOCK_SIZE * 2];
    assert_eq!(
        device.read_block(7, &mut two_blocks).unwrap_err(),
        DriverError::InvalidInput
    );
    assert_eq!(
        device.write_block(0, &[0; BLOCK_SIZE - 1]).unwrap_err(),
        DriverError::InvalidInput
    );
    assert!(device.read_block(8, &mut []).is_ok());
    del_gendisk(disk.device_number()).expect("remove bounds disk");
}

#[def_test(serial)]
fn set_capacity_updates_the_resident_block_device() {
    let disk = disk("test-capacity", 242, 0, 1);
    let device = add_disk(disk.clone()).expect("publish capacity disk");
    assert_eq!(device.num_sectors(), 8);
    device.set_capacity(3).expect("valid capacity");
    assert_eq!(device.num_sectors(), 3);
    assert_eq!(device.size(), 3 * BLOCK_SIZE as u64);
    del_gendisk(disk.device_number()).expect("remove capacity disk");
}

#[def_test]
fn gendisk_rejects_invalid_capacity_geometry() {
    let zero_block_size = Gendisk::alloc(
        String::from("zero-block-size"),
        245,
        0,
        1,
        Box::new(GeometryDisk {
            blocks: 1,
            block_size: 0,
        }),
    );
    assert!(matches!(zero_block_size, Err(DriverError::InvalidInput)));

    let overflowing_size = Gendisk::alloc(
        String::from("overflowing-size"),
        245,
        0,
        1,
        Box::new(GeometryDisk {
            blocks: u64::MAX,
            block_size: BLOCK_SIZE,
        }),
    );
    assert!(matches!(overflowing_size, Err(DriverError::InvalidInput)));

    let oversized_block = Gendisk::alloc(
        String::from("oversized-logical-block"),
        245,
        0,
        1,
        Box::new(GeometryDisk {
            blocks: 1,
            block_size: 8192,
        }),
    );
    assert!(matches!(oversized_block, Err(DriverError::InvalidInput)));
}

#[def_test]
fn four_kib_logical_io_uses_sector_capacity() {
    let disk = Gendisk::alloc(
        String::from("four-k-logical-blocks"),
        247,
        0,
        1,
        Box::new(MemoryDisk::with_block_size(2, 4096)),
    )
    .expect("valid 4-KiB logical geometry");

    let limits = disk.queue().limits();
    assert_eq!(limits.logical_block_size(), 4096);
    assert_eq!(limits.physical_block_size(), 4096);
    assert_eq!(limits.io_min(), 4096);
    assert_eq!(limits.io_opt(), 0);
    assert_eq!(limits.alignment_offset(), 0);
    assert_eq!(disk.part0().num_sectors(), 16);
    assert_eq!(disk.part0().size(), 8192);

    let written = vec![0x5a; 4096];
    disk.part0()
        .write_block(1, &written)
        .expect("write second logical block");
    let mut read = vec![0; 4096];
    disk.part0()
        .read_block(1, &mut read)
        .expect("read second logical block");
    assert_eq!(read, written);
    assert_eq!(
        disk.part0().read_block(2, &mut read).unwrap_err(),
        DriverError::InvalidInput
    );

    disk.part0()
        .set_capacity(9)
        .expect("sector capacity need not fill a logical block");
    assert_eq!(disk.part0().size(), 9 * u64::from(SECTOR_SIZE));
    assert_eq!(
        BlockDeviceOperations::read_block(disk.part0().as_ref(), 1, &mut read).unwrap_err(),
        DriverError::InvalidInput
    );
}

#[def_test(serial)]
fn existing_part0_outlives_unpublication() {
    let disk = disk("test-unpublished-lifetime", 248, 0, 1);
    let device = add_disk(disk.clone()).expect("publish lifetime test disk");
    let disk_weak = Arc::downgrade(&device.disk);
    let part0_weak = Arc::downgrade(&device);
    let queue_weak = Arc::downgrade(device.queue());
    let disk_identity = device.disk() as *const Gendisk;
    let removed = del_gendisk(disk.device_number()).expect("unpublish lifetime test disk");
    assert!(core::ptr::eq(&*removed, &*disk));
    drop(removed);
    drop(disk);

    assert!(lookup_block_device(device.device_number()).is_none());
    assert!(core::ptr::eq(device.disk(), disk_identity));
    assert!(Arc::ptr_eq(&device.disk().part0(), &device));
    assert!(Arc::ptr_eq(device.disk().queue(), device.queue()));
    let mut data = vec![0; BLOCK_SIZE];
    device
        .read_block(0, &mut data)
        .expect("existing part0 keeps its disk alive");
    device
        .set_disk_read_only(true)
        .expect("disk state is still available");
    assert_eq!(device.write_block(0, &data), Err(DriverError::ReadOnly));
    drop(device);
    assert!(part0_weak.upgrade().is_none());
    assert!(disk_weak.upgrade().is_none());
    assert!(queue_weak.upgrade().is_none());
}

#[def_test(serial)]
fn read_only_state_is_owned_by_the_block_core() {
    let disk = disk("test-read-only", 244, 0, 1);
    let device = add_disk(disk.clone()).expect("publish read-only disk");
    use linux_raw_sys::ioctl::BLKROSET;

    assert_eq!(
        device.as_ref().ioctl(
            BlockOpenMode::READ,
            BLKROSET,
            0,
            false,
            || panic!("authorization must precede argument access"),
            |_| panic!("setter must not copy out"),
        ),
        Err(KError::PermissionDenied)
    );
    assert!(!device.is_read_only());
    assert_eq!(
        device.as_ref().ioctl(
            BlockOpenMode::READ,
            BLKROSET,
            0,
            true,
            || Err(KError::BadAddress),
            |_| panic!("setter must not copy out"),
        ),
        Err(KError::BadAddress)
    );
    assert!(!device.is_read_only());
    for value in [-1, 2] {
        device
            .as_ref()
            .ioctl(
                BlockOpenMode::READ,
                BLKROSET,
                0,
                true,
                || Ok(value),
                |_| panic!("setter must not copy out"),
            )
            .expect("nonzero int sets disk read-only");
        assert!(device.is_read_only());
    }
    assert!(device.is_read_only());
    assert_eq!(
        device.write_block(0, &[0; BLOCK_SIZE]).unwrap_err(),
        DriverError::ReadOnly
    );
    assert_eq!(
        BlockDeviceOperations::write_block(device.as_ref(), 0, &[0; BLOCK_SIZE]).unwrap_err(),
        DriverError::ReadOnly
    );
    device
        .as_ref()
        .ioctl(
            BlockOpenMode::READ,
            BLKROSET,
            0,
            true,
            || Ok(0),
            |_| panic!("setter must not copy out"),
        )
        .expect("zero clears administrative read-only state");
    assert!(device.write_block(0, &[0; BLOCK_SIZE]).is_ok());
    del_gendisk(disk.device_number()).expect("remove read-only disk");
}

#[def_test(serial)]
fn inherently_read_only_disk_cannot_be_made_writable() {
    let disk = Gendisk::alloc(
        String::from("test-inherently-read-only"),
        246,
        0,
        1,
        Box::new(InherentlyReadOnlyDisk(MemoryDisk::new(8))),
    )
    .expect("valid inherently read-only disk");
    let device = add_disk(disk.clone()).expect("publish inherently read-only disk");

    assert!(device.is_read_only());
    assert!(matches!(
        device.as_ref().ioctl(
            BlockOpenMode::READ,
            linux_raw_sys::ioctl::BLKROSET,
            0,
            true,
            || Ok(0),
            |_| panic!("setter must not copy out"),
        ),
        Ok(0)
    ));
    assert!(device.is_read_only());
    assert_eq!(
        device.write_block(0, &[0; BLOCK_SIZE]).unwrap_err(),
        DriverError::ReadOnly
    );

    del_gendisk(disk.device_number()).expect("remove inherently read-only disk");
}

#[def_test(serial)]
fn exclusive_claim_follows_token_lifetime() {
    let disk = disk("test-exclusive-claim", 243, 0, 1);
    let device = add_disk(disk.clone()).expect("publish claim test disk");

    let first = device.claim_exclusive().expect("claim unowned device");
    assert!(matches!(
        device.claim_exclusive(),
        Err(KError::ResourceBusy)
    ));

    first.release();
    let second = device
        .claim_exclusive()
        .expect("explicit release makes device claimable");
    drop(second);
    device
        .claim_exclusive()
        .expect("drop makes device claimable");

    del_gendisk(disk.device_number()).expect("remove claim test disk");
}
