// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, string::String, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::{
    MAX_LFS_FILESIZE, StatFs, SuperBlock, SuperBlockFlags, SuperBlockOperations,
    super_block_registry,
};
use crate::{
    Dentry, FileSystemType, FsContext, FsContextOperations, NodeFlags, NodePermission, NodeType,
    Path, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult,
};

struct TestSuperOperations;

static TEST_SUPER_OPERATIONS: TestSuperOperations = TestSuperOperations;
static TEST_FILE_SYSTEM_TYPE: crate::FileSystemType =
    crate::FileSystemType::internal("private-test");

struct TestPrivate(u64);

impl SuperBlockOperations for TestSuperOperations {
    fn statfs(&self, super_block: &SuperBlock) -> crate::VfsResult<StatFs> {
        if super_block.private::<TestPrivate>()?.0 != 0x5f5f_494e_464f {
            return Err(crate::VfsError::InvalidInput);
        }
        Ok(StatFs {
            fs_type: 0,
            block_size: 4096,
            blocks: 0,
            blocks_free: 0,
            blocks_available: 0,
            file_count: 0,
            free_file_count: 0,
            name_length: 255,
            fragment_size: 4096,
        })
    }
}

#[def_test]
fn superblock_separates_operations_from_private_state() {
    let super_block = SuperBlock::try_new_with_flags_and_private(
        &TEST_FILE_SYSTEM_TYPE,
        &TEST_SUPER_OPERATIONS,
        TestPrivate(0x5f5f_494e_464f),
        SuperBlockFlags::empty(),
        4096,
        1 << 30,
        |_| {
            let inode = VfsInode::new_dir_with_defaults(
                NodeFlags::empty(),
                VfsInodeInit::new(
                    2,
                    0,
                    Umode::new(NodeType::Directory, NodePermission::default()),
                ),
            );
            Ok::<_, crate::VfsError>(Dentry::new_dir_from_inode(inode, None, String::new()))
        },
    )
    .expect("construct superblock with private state");

    assert_eq!(super_block.block_size(), 4096);
    assert_eq!(super_block.max_file_size(), 1 << 30);
    assert!(super_block.private::<TestSuperOperations>().is_err());
    assert!(super_block.stat().is_ok());
}

static FILL_COUNT: AtomicUsize = AtomicUsize::new(0);
static TRANSITION_STAGE: AtomicUsize = AtomicUsize::new(0);
static TRANSITION_WAITER_STARTED: AtomicUsize = AtomicUsize::new(0);
static TRANSITION_WAITER_DONE: AtomicUsize = AtomicUsize::new(0);
static TRANSITION_WAITERS: ktask::WaitQueue = ktask::WaitQueue::new();

fn bdev_test_get_tree(
    _context: &mut FsContext<'_>,
    _lookup_root: &Path,
    _lookup_pwd: &Path,
) -> VfsResult<Arc<SuperBlock>> {
    unreachable!("the registry test invokes its fill operation directly")
}

static BDEV_TEST_CONTEXT_OPERATIONS: FsContextOperations =
    FsContextOperations::new(bdev_test_get_tree);

fn init_bdev_test_context(context: &mut FsContext<'_>) -> VfsResult<()> {
    context.set_operations(&BDEV_TEST_CONTEXT_OPERATIONS);
    Ok(())
}

static BDEV_TEST_FILE_SYSTEM_TYPE: FileSystemType =
    FileSystemType::device_backed("bdev-test", init_bdev_test_context);
static BDEV_ALT_FILE_SYSTEM_TYPE: FileSystemType =
    FileSystemType::device_backed("bdev-alt-test", init_bdev_test_context);

struct BdevTestSuperOperations;

static BDEV_TEST_SUPER_OPERATIONS: BdevTestSuperOperations = BdevTestSuperOperations;

fn test_statfs() -> StatFs {
    StatFs {
        fs_type: 0,
        block_size: 512,
        blocks: 1,
        blocks_free: 1,
        blocks_available: 1,
        file_count: 1,
        free_file_count: 0,
        name_length: 255,
        fragment_size: 512,
    }
}

impl SuperBlockOperations for BdevTestSuperOperations {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
        Ok(test_statfs())
    }
}

struct BlockingShutdownOperations;

static BLOCKING_SHUTDOWN_OPERATIONS: BlockingShutdownOperations = BlockingShutdownOperations;

impl SuperBlockOperations for BlockingShutdownOperations {
    fn statfs(&self, _super_block: &SuperBlock) -> VfsResult<StatFs> {
        Ok(test_statfs())
    }

    fn sync_fs(&self, _super_block: &SuperBlock) -> VfsResult<()> {
        if TRANSITION_STAGE
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            TRANSITION_WAITERS.notify_all(false);
            TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) >= 2);
        }
        Ok(())
    }
}

struct TestBlockDevice;

impl block::BlockDeviceOperations for TestBlockDevice {
    fn num_blocks(&self) -> u64 {
        1
    }

    fn block_size(&self) -> usize {
        512
    }

    fn read_block(&self, _block_id: u64, _buf: &mut [u8]) -> block::DriverResult {
        Ok(())
    }

    fn write_block(&self, _block_id: u64, _buf: &[u8]) -> block::DriverResult {
        Ok(())
    }

    fn flush(&self) -> block::DriverResult {
        Ok(())
    }
}

fn initialize_test_bdev(
    super_block: &Arc<SuperBlock>,
    operations: &'static dyn SuperBlockOperations,
) -> VfsResult<()> {
    super_block.initialize(operations, 512, MAX_LFS_FILESIZE, |super_block| {
        let inode = super_block.get_or_init_inode(1, || {
            VfsInode::new_dir_with_defaults(
                NodeFlags::empty(),
                VfsInodeInit::new(
                    1,
                    0,
                    Umode::new(NodeType::Directory, NodePermission::default()),
                ),
            )
        });
        Ok(Dentry::new_dir_from_inode(inode, None, String::new()))
    })
}

fn fill_test_bdev(super_block: &Arc<SuperBlock>) -> VfsResult<()> {
    FILL_COUNT.fetch_add(1, Ordering::Relaxed);
    initialize_test_bdev(super_block, &BDEV_TEST_SUPER_OPERATIONS)
}

fn fail_test_bdev(_super_block: &Arc<SuperBlock>) -> VfsResult<()> {
    FILL_COUNT.fetch_add(1, Ordering::Relaxed);
    TRANSITION_STAGE.store(1, Ordering::Release);
    TRANSITION_WAITERS.notify_all(false);
    TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) >= 2);
    Err(VfsError::InvalidInput)
}

fn fill_blocking_shutdown_bdev(super_block: &Arc<SuperBlock>) -> VfsResult<()> {
    FILL_COUNT.fetch_add(1, Ordering::Relaxed);
    initialize_test_bdev(super_block, &BLOCKING_SHUTDOWN_OPERATIONS)
}

fn reset_transition_test_state() {
    FILL_COUNT.store(0, Ordering::Relaxed);
    TRANSITION_STAGE.store(0, Ordering::Release);
    TRANSITION_WAITER_STARTED.store(0, Ordering::Release);
    TRANSITION_WAITER_DONE.store(0, Ordering::Release);
}

fn spawn_successful_waiting_mount(block_device: Arc<block::BlockDevice>) -> ktask::KtaskRef {
    ktask::spawn(move || {
        TRANSITION_WAITER_STARTED.store(1, Ordering::Release);
        TRANSITION_WAITERS.notify_all(false);
        let super_block = super_block_registry()
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device,
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .expect("mount retries after the old superblock transition");
        super_block.activate_mount();
        super_block.deactivate_mount();
        TRANSITION_WAITER_DONE.store(1, Ordering::Release);
        TRANSITION_WAITERS.notify_all(false);
    })
}

fn add_test_disk(name: &str, major: u32) -> (block::GendiskRef, Arc<block::BlockDevice>) {
    let disk = block::Gendisk::alloc(String::from(name), major, 0, 1, Box::new(TestBlockDevice))
        .expect("valid superblock test disk");
    let block_device = block::add_disk(disk.clone()).expect("publish superblock test disk");
    (disk, block_device)
}

#[def_test(serial)]
fn sget_dev_reuses_identity_and_rejects_readonly_mismatch() {
    FILL_COUNT.store(0, Ordering::Relaxed);
    let (disk, block_device) = add_test_disk("kvfs-sget-test", 246);
    let registry = super_block_registry();

    let first = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("first mount initializes the device superblock");
    let second = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("second mount reuses the device superblock");

    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);
    first.activate_mount();
    assert!(matches!(
        registry.get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device,
            SuperBlockFlags::RDONLY,
            fill_test_bdev,
        ),
        Err(VfsError::ResourceBusy)
    ));
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);

    first.deactivate_mount();
    drop(second);
    block::del_gendisk(disk.device_number()).expect("remove sget test disk");
}

#[def_test(serial)]
fn new_file_readahead_snapshots_the_backing_disk_budget() {
    struct FileOps;
    impl crate::FileOperations for FileOps {}

    let (disk, device) = add_test_disk("kvfs-ra-budget", 250);
    let sb = super_block_registry()
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .unwrap();
    let mount = crate::Mount::new_root(&sb);
    let open = || {
        let mut builder = crate::VfsFileBuilder::from_path_state(
            mount.root_path(),
            crate::FMode::READ,
            crate::OpenFlags::empty(),
            Arc::new(FileOps),
            kcred::initial_cred(),
        );
        builder.mark_opened().unwrap();
        builder.finish().unwrap()
    };
    let set = |sectors| {
        device
            .as_ref()
            .ioctl(
                block::BlockOpenMode::READ,
                linux_raw_sys::ioctl::BLKRASET,
                sectors,
                true,
                || panic!("setter uses an immediate value"),
                |_| panic!("setter has no output"),
            )
            .unwrap()
    };
    set(8);
    let first = open();
    assert_eq!(first.readahead.lock().on_miss(0, 1, |_| 0).0, 0..1);
    set(64);
    let second = open();
    assert_eq!(second.readahead.lock().on_miss(0, 1, |_| 0).0, 0..2);
    assert_eq!(first.readahead.lock().on_miss(100, 8, |_| 0).0, 100..101);
    set(0);
    assert!(!open().readahead.lock().is_enabled());
    assert!(first.readahead.lock().is_enabled());
    drop(first);
    drop(second);
    drop(mount);
    drop(sb);
    block::del_gendisk(disk.device_number()).unwrap();
}

#[def_test(serial)]
fn block_device_claim_rejects_a_different_filesystem_until_shutdown() {
    FILL_COUNT.store(0, Ordering::Relaxed);
    let (disk, block_device) = add_test_disk("kvfs-holder-test", 247);
    let registry = super_block_registry();
    let first = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("first filesystem claims the device");
    first.activate_mount();

    assert!(matches!(
        registry.get_or_try_init_bdev(
            &BDEV_ALT_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::empty(),
            fill_test_bdev,
        ),
        Err(VfsError::ResourceBusy)
    ));
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 1);

    first.deactivate_mount();
    let replacement = registry
        .get_or_try_init_bdev(
            &BDEV_ALT_FILE_SYSTEM_TYPE,
            block_device,
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("dead superblock releases its block-device claim");
    replacement.activate_mount();
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
    replacement.deactivate_mount();
    block::del_gendisk(disk.device_number()).expect("remove holder test disk");
}

#[def_test(serial)]
fn sget_dev_uses_canonical_device_object_identity() {
    FILL_COUNT.store(0, Ordering::Relaxed);
    let (old_disk, old_device) = add_test_disk("kvfs-old-device", 248);
    let registry = super_block_registry();
    let old_super_block = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            old_device,
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("mount old device generation");
    old_super_block.activate_mount();
    block::del_gendisk(old_disk.device_number()).expect("unpublish old disk");

    let (new_disk, new_device) = add_test_disk("kvfs-new-device", 248);
    let new_super_block = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            new_device,
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("mount replacement device generation");
    new_super_block.activate_mount();

    assert!(!Arc::ptr_eq(&old_super_block, &new_super_block));
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);

    old_super_block.deactivate_mount();
    new_super_block.deactivate_mount();
    block::del_gendisk(new_disk.device_number()).expect("remove replacement disk");
}

#[def_test(serial)]
fn read_only_block_device_cannot_be_reconfigured_read_write() {
    FILL_COUNT.store(0, Ordering::Relaxed);
    let (disk, block_device) = add_test_disk("kvfs-read-only-remount", 249);
    block_device
        .set_disk_read_only(true)
        .expect("make backing device read-only");
    let super_block = super_block_registry()
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device,
            SuperBlockFlags::RDONLY,
            fill_test_bdev,
        )
        .expect("mount read-only device read-only");
    super_block.activate_mount();
    let cred = kcred::initial_cred();
    let mut context = FsContext::new_reconfigure(
        super_block.as_ref(),
        None,
        None,
        SuperBlockFlags::empty(),
        SuperBlockFlags::RDONLY,
        &cred,
    )
    .expect("construct writable reconfigure context");

    assert_eq!(
        super_block.reconfigure(&mut context),
        Err(VfsError::PermissionDenied)
    );
    assert!(super_block.flags().contains(SuperBlockFlags::RDONLY));

    super_block.deactivate_mount();
    block::del_gendisk(disk.device_number()).expect("remove read-only remount disk");
}

#[def_test(serial)]
fn failed_fill_wakes_waiter_and_allows_retry() {
    reset_transition_test_state();
    let (disk, block_device) = add_test_disk("kvfs-failed-fill", 250);
    let owner_device = block_device.clone();
    let owner = ktask::spawn(move || {
        match super_block_registry().get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            owner_device,
            SuperBlockFlags::empty(),
            fail_test_bdev,
        ) {
            Err(VfsError::InvalidInput) => {}
            result => panic!("failed fill returned an unexpected result: {result:?}"),
        }
    });
    TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) == 1);

    let waiter = spawn_successful_waiting_mount(block_device);
    TRANSITION_WAITERS.wait_until(|| TRANSITION_WAITER_STARTED.load(Ordering::Acquire) == 1);
    ktask::yield_now();
    assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 0);

    TRANSITION_STAGE.store(2, Ordering::Release);
    TRANSITION_WAITERS.notify_all(true);
    owner.join();
    waiter.join();

    assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 1);
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
    block::del_gendisk(disk.device_number()).expect("remove failed-fill test disk");
}

#[def_test(serial)]
fn dying_superblock_wakes_waiter_after_shutdown() {
    reset_transition_test_state();
    let (disk, block_device) = add_test_disk("kvfs-dying-wait", 251);
    let super_block = super_block_registry()
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::empty(),
            fill_blocking_shutdown_bdev,
        )
        .expect("mount device before blocking shutdown");
    super_block.activate_mount();

    let shutting_down = super_block.clone();
    let shutdown = ktask::spawn(move || shutting_down.deactivate_mount());
    TRANSITION_WAITERS.wait_until(|| TRANSITION_STAGE.load(Ordering::Acquire) == 1);

    let waiter = spawn_successful_waiting_mount(block_device);
    TRANSITION_WAITERS.wait_until(|| TRANSITION_WAITER_STARTED.load(Ordering::Acquire) == 1);
    ktask::yield_now();
    assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 0);

    TRANSITION_STAGE.store(2, Ordering::Release);
    TRANSITION_WAITERS.notify_all(true);
    shutdown.join();
    waiter.join();

    assert_eq!(TRANSITION_WAITER_DONE.load(Ordering::Acquire), 1);
    assert_eq!(FILL_COUNT.load(Ordering::Relaxed), 2);
    block::del_gendisk(disk.device_number()).expect("remove dying-wait test disk");
}
