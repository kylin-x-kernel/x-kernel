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

struct SyncFailureProbe {
    calls: crate::Mutex<(usize, usize, usize)>,
    has_data_error: bool,
}

struct FailingSyncOperations;

static FAILING_SYNC_OPERATIONS: FailingSyncOperations = FailingSyncOperations;

impl crate::InodeOperations for FailingSyncOperations {}
impl crate::FileOperations for FailingSyncOperations {}

impl crate::AddressSpaceOperations for FailingSyncOperations {
    fn read_folio(
        &self,
        _mapping: &crate::AddressSpace,
        mut folio: pagecache::FolioRead,
    ) -> VfsResult<()> {
        folio.data().fill(0);
        folio.complete(Ok(()))
    }

    fn writepages(
        &self,
        mapping: &crate::AddressSpace,
        _control: &mut crate::WritebackControl,
    ) -> VfsResult<()> {
        let sb = mapping.inode().unwrap().super_block()?;
        let probe = sb.private::<SyncFailureProbe>()?;
        probe.calls.lock().0 += 1;
        if probe.has_data_error {
            Err(VfsError::Io)
        } else {
            Ok(())
        }
    }
}

impl SuperBlockOperations for FailingSyncOperations {
    fn statfs(&self, _sb: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
        Err(VfsError::Unsupported)
    }

    fn write_inode(
        &self,
        inode: &VfsInode,
        _control: &mut crate::WritebackControl,
    ) -> VfsResult<()> {
        if inode.node_type() != NodeType::RegularFile {
            return Ok(());
        }
        inode
            .super_block()?
            .private::<SyncFailureProbe>()?
            .calls
            .lock()
            .1 += 1;
        Err(VfsError::StorageFull)
    }

    fn sync_fs(&self, sb: &SuperBlock) -> VfsResult<()> {
        sb.private::<SyncFailureProbe>()?.calls.lock().2 += 1;
        Err(VfsError::InvalidData)
    }
}

#[def_test]
fn filesystem_sync_attempts_all_stages_and_keeps_the_first_error() {
    for has_data_error in [true, false] {
        let sb = SuperBlock::try_new_with_flags_and_private(
            &TEST_FILE_SYSTEM_TYPE,
            &FAILING_SYNC_OPERATIONS,
            kalloc_types::KBox::new(SyncFailureProbe {
                calls: crate::Mutex::new((0, 0, 0)),
                has_data_error,
            })
            .unwrap(),
            SuperBlockFlags::empty(),
            4096,
            MAX_LFS_FILESIZE,
            |_| {
                Ok::<_, VfsError>(Dentry::new_dir_from_inode(
                    VfsInode::new_dir_with_defaults(
                        NodeFlags::empty(),
                        VfsInodeInit::new(
                            2,
                            0,
                            Umode::new(NodeType::Directory, NodePermission::default()),
                        ),
                    ),
                    None,
                    String::new(),
                ))
            },
        )
        .unwrap();
        let inodes = [3, 4].map(|number| {
            let inode = VfsInode::new_file_with_address_space(
                Arc::new(FailingSyncOperations),
                &FAILING_SYNC_OPERATIONS,
                VfsInodeInit::new(
                    number,
                    4096,
                    Umode::new(NodeType::RegularFile, NodePermission::default()),
                ),
            );
            inode.bind_super_block(&sb);
            inode
                .address_space()
                .with_folio_or_create(0, |folio| {
                    folio.mark_dirty();
                    Ok(())
                })
                .unwrap();
            inode
        });
        assert_eq!(
            sb.sync_fs_locked(),
            Err(if has_data_error {
                VfsError::Io
            } else {
                VfsError::StorageFull
            })
        );
        assert_eq!(
            *sb.private::<SyncFailureProbe>().unwrap().calls.lock(),
            (2, 2, 1)
        );
        drop(inodes);
    }
}

impl SuperBlockOperations for TestSuperOperations {
    fn statfs(&self, super_block: &SuperBlock, _dentry: &crate::Dentry) -> crate::VfsResult<StatFs> {
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
        kalloc_types::KBox::new(TestPrivate(0x5f5f_494e_464f)).unwrap(),
        SuperBlockFlags::NOATIME,
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
    super_block.mark_read_only();
    super_block.mark_read_only();
    assert_eq!(
        super_block.flags(),
        SuperBlockFlags::NOATIME | SuperBlockFlags::RDONLY
    );
}

static FILL_COUNT: AtomicUsize = AtomicUsize::new(0);

struct PrivateDropProbe(Arc<AtomicUsize>);

impl Drop for PrivateDropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn failed_root_initialization_releases_prepared_private_owner() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = kalloc_types::KBox::new(PrivateDropProbe(drops.clone())).unwrap();
    let saw_private = core::cell::Cell::new(false);
    let result = SuperBlock::try_new_with_flags_and_private(
        &TEST_FILE_SYSTEM_TYPE,
        &TEST_SUPER_OPERATIONS,
        owner,
        SuperBlockFlags::empty(),
        4096,
        MAX_LFS_FILESIZE,
        |super_block| {
            saw_private.set(super_block.private::<PrivateDropProbe>().is_ok());
            Err::<Dentry, _>(VfsError::NoMemory)
        },
    );
    assert!(matches!(result, Err(VfsError::NoMemory)));
    assert!(saw_private.get());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn prepared_private_owner_keeps_address_until_superblock_teardown() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = kalloc_types::KBox::new(PrivateDropProbe(drops.clone())).unwrap();
    let address = core::ptr::from_ref(&*owner);
    let super_block = SuperBlock::new_with_flags_and_private(
        &TEST_FILE_SYSTEM_TYPE,
        &TEST_SUPER_OPERATIONS,
        owner,
        SuperBlockFlags::empty(),
        4096,
        MAX_LFS_FILESIZE,
        |_| {
            Dentry::new_dir_from_inode(
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        1,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                ),
                None,
                String::new(),
            )
        },
    );
    assert_eq!(
        core::ptr::from_ref(super_block.private::<PrivateDropProbe>().unwrap()),
        address,
    );
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(super_block);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
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
    fn statfs(&self, _super_block: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
        Ok(test_statfs())
    }
}

struct BlockingShutdownOperations;

static BLOCKING_SHUTDOWN_OPERATIONS: BlockingShutdownOperations = BlockingShutdownOperations;

impl SuperBlockOperations for BlockingShutdownOperations {
    fn statfs(&self, _super_block: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
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
}
impl TestBlockDevice {
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

impl block::BioOperations for TestBlockDevice {
    fn submit_bio(&self, mut bio: block::Bio) {
        let block_id = bio.sector() / (block::BlockDeviceOperations::block_size(self) as u64 / 512);
        let result = match bio.operation() {
            block::BioOperation::Read => {
                bio.read_contiguous(|bytes| self.read_block(block_id, bytes))
            }
            block::BioOperation::Write => {
                bio.write_contiguous(|bytes| self.write_block(block_id, bytes))
            }
            block::BioOperation::Flush => self.flush(),
        };
        bio.end_io(result);
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

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: Gendisk::alloc still requires a standard Box driver owner; keep this fixture on the production ownership contract"
)]
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
    // Linux `get_tree_bdev()` refuses to change an extant superblock's RO
    // state in either direction, so a read-only request over the live
    // writable superblock is EBUSY (`ResourceBusy`).
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
fn block_device_claim_rejects_a_different_filesystem_until_last_holder_close() {
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
    let nested = block_device
        .open(block::BlockOpenMode::READ, Some(first.clone()))
        .expect("the superblock is the holder of its open file");

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
    nested.release();
    let replacement = registry
        .get_or_try_init_bdev(
            &BDEV_ALT_FILE_SYSTEM_TYPE,
            block_device,
            SuperBlockFlags::empty(),
            fill_test_bdev,
        )
        .expect("the last holder close releases the block-device claim");
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

/// The single-direction rule keeps the OTHER direction: a read-only
/// superblock still cannot back a writable mount request (Linux refuses to
/// widen an existing read-only superblock either).
#[def_test(serial)]
fn read_only_superblock_rejects_a_writable_request() {
    let (disk, block_device) = add_test_disk("kvfs-ro-sb-rw-request", 252);
    let registry = super_block_registry();
    let read_only = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            block_device.clone(),
            SuperBlockFlags::RDONLY,
            fill_test_bdev,
        )
        .expect("read-only request initializes the superblock");
    assert!(read_only.flags().contains(SuperBlockFlags::RDONLY));
    read_only.activate_mount();

    assert_eq!(
        registry
            .get_or_try_init_bdev(
                &BDEV_TEST_FILE_SYSTEM_TYPE,
                block_device,
                SuperBlockFlags::empty(),
                fill_test_bdev,
            )
            .err(),
        Some(VfsError::ResourceBusy)
    );

    read_only.deactivate_mount();
    block::del_gendisk(disk.device_number()).expect("remove ro-sb test disk");
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

struct OpenLifecycleDevice {
    modes: crate::Mutex<alloc::vec::Vec<block::BlockOpenMode>>,
    releases: AtomicUsize,
    fail_open: core::sync::atomic::AtomicBool,
}

impl block::BlockDeviceOperations for OpenLifecycleDevice {
    fn num_blocks(&self) -> u64 {
        8
    }

    fn block_size(&self) -> usize {
        512
    }

    fn open(&self, _disk: &block::Gendisk, mode: block::BlockOpenMode) -> VfsResult<()> {
        // A driver may inspect VFS state; no global registry lock may span it.
        let _ = super_block_registry().live_super_blocks();
        self.modes.lock().push(mode);
        if self.fail_open.load(Ordering::Relaxed) {
            Err(VfsError::Io)
        } else {
            Ok(())
        }
    }

    fn release(&self, _disk: &block::Gendisk) {
        let _ = super_block_registry().live_super_blocks();
        self.releases.fetch_add(1, Ordering::Relaxed);
    }
}

impl block::BioOperations for OpenLifecycleDevice {
    fn submit_bio(&self, bio: block::Bio) {
        bio.end_io(Ok(()));
    }
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: Gendisk::alloc still requires a standard Box driver owner; keep this fixture on the production ownership contract"
)]
fn mount_opens_once_and_closes_at_failure_or_logical_shutdown() {
    let backend = Arc::new(OpenLifecycleDevice {
        modes: crate::Mutex::new(alloc::vec::Vec::new()),
        releases: AtomicUsize::new(0),
        fail_open: core::sync::atomic::AtomicBool::new(true),
    });
    let device = block::Gendisk::alloc(
        String::from("mount-open"),
        0,
        0,
        1,
        Box::new(backend.clone()),
    )
    .unwrap()
    .part0();
    let registry = super_block_registry();
    let mut did_fill = false;
    assert!(matches!(
        registry.get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            |_| {
                did_fill = true;
                Ok(())
            }
        ),
        Err(VfsError::Io)
    ));
    assert!(!did_fill);
    assert_eq!(backend.releases.load(Ordering::Relaxed), 0);
    backend.fail_open.store(false, Ordering::Relaxed);
    assert!(matches!(
        registry.get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            |_| Err(VfsError::InvalidInput)
        ),
        Err(VfsError::InvalidInput)
    ));
    assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
    let sb = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            |sb| initialize_test_bdev(sb, &BDEV_TEST_SUPER_OPERATIONS),
        )
        .unwrap();
    sb.activate_mount();
    let shared = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            |_| panic!("shared mount must not fill again"),
        )
        .unwrap();
    assert!(Arc::ptr_eq(&sb, &shared));
    shared.activate_mount();
    assert_eq!(backend.modes.lock().len(), 3);
    sb.deactivate_mount();
    assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
    shared.deactivate_mount();
    assert_eq!(backend.releases.load(Ordering::Relaxed), 2);
    let readonly = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::RDONLY,
            |sb| initialize_test_bdev(sb, &BDEV_TEST_SUPER_OPERATIONS),
        )
        .unwrap();
    readonly.activate_mount();
    let read_mode = block::BlockOpenMode::READ | block::BlockOpenMode::EXCLUSIVE;
    let write_mode = read_mode | block::BlockOpenMode::WRITE;
    {
        let modes = backend.modes.lock();
        assert_eq!(
            modes.as_slice(),
            &[write_mode, write_mode, write_mode, read_mode]
        );
    }
    readonly.deactivate_mount();
    drop((sb, shared, readonly));
    assert_eq!(backend.releases.load(Ordering::Relaxed), 3);

    device.set_disk_read_only(true).unwrap();
    did_fill = false;
    assert!(matches!(
        registry.get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device,
            SuperBlockFlags::empty(),
            |_| {
                did_fill = true;
                Ok(())
            }
        ),
        Err(VfsError::PermissionDenied)
    ));
    assert!(!did_fill);
    assert_eq!(backend.releases.load(Ordering::Relaxed), 4);
}

#[def_test(serial)]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: Gendisk::alloc still requires a standard Box driver owner; keep this fixture on the production ownership contract"
)]
fn registry_lookup_defers_final_superblock_drop_until_after_unlock() {
    let backend = Arc::new(OpenLifecycleDevice {
        modes: crate::Mutex::new(alloc::vec::Vec::new()),
        releases: AtomicUsize::new(0),
        fail_open: core::sync::atomic::AtomicBool::new(false),
    });
    let device = block::Gendisk::alloc(
        String::from("lookup-drop"),
        0,
        0,
        1,
        Box::new(backend.clone()),
    )
    .unwrap()
    .part0();
    let registry = super_block_registry();
    let sb = registry
        .get_or_try_init_bdev(
            &BDEV_TEST_FILE_SYSTEM_TYPE,
            device.clone(),
            SuperBlockFlags::empty(),
            |sb| initialize_test_bdev(sb, &BDEV_TEST_SUPER_OPERATIONS),
        )
        .unwrap();
    let mut live_refs = alloc::vec::Vec::new();
    let mut entries = registry.super_blocks.lock();
    assert!(
        entries
            .lookup_bdev(&BDEV_ALT_FILE_SYSTEM_TYPE, &device, &mut live_refs)
            .is_none()
    );
    drop(sb);
    assert_eq!(backend.releases.load(Ordering::Relaxed), 0);
    drop(entries);
    drop(live_refs);
    assert_eq!(backend.releases.load(Ordering::Relaxed), 1);
}

struct FinalReleaseOperations;
static FINAL_RELEASE_OPERATIONS: FinalReleaseOperations = FinalReleaseOperations;

struct FinalReleaseState {
    syncs: AtomicUsize,
    releases: AtomicUsize,
}

impl SuperBlockOperations for FinalReleaseOperations {
    fn statfs(&self, _super_block: &SuperBlock, _dentry: &crate::Dentry) -> VfsResult<StatFs> {
        Ok(test_statfs())
    }

    fn sync_fs(&self, super_block: &SuperBlock) -> VfsResult<()> {
        super_block
            .private::<Arc<FinalReleaseState>>()?
            .syncs
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn put_super(&self, super_block: &SuperBlock) -> VfsResult<()> {
        let state = super_block.private::<Arc<FinalReleaseState>>()?;
        if state.syncs.load(Ordering::Relaxed) != 2 {
            return Err(VfsError::InvalidData);
        }
        state.releases.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[def_test]
fn final_resource_release_follows_both_shutdown_sync_passes() {
    let state = Arc::new(FinalReleaseState {
        syncs: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
    });
    let super_block = SuperBlock::new_with_flags_and_private(
        &TEST_FILE_SYSTEM_TYPE,
        &FINAL_RELEASE_OPERATIONS,
        kalloc_types::KBox::new(state.clone()).unwrap(),
        SuperBlockFlags::empty(),
        4096,
        MAX_LFS_FILESIZE,
        |_| {
            Dentry::new_dir_from_inode(
                VfsInode::new_dir_with_defaults(
                    NodeFlags::empty(),
                    VfsInodeInit::new(
                        2,
                        0,
                        Umode::new(NodeType::Directory, NodePermission::default()),
                    ),
                ),
                None,
                String::new(),
            )
        },
    );
    super_block.activate_mount();
    super_block.deactivate_mount();
    assert_eq!(state.releases.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn fifo_timestamp_mutation_waits_for_readonly_policy_publication() {
    struct SpecialOperations;
    impl crate::InodeOperations for SpecialOperations {}

    let sb = SuperBlock::new(&TEST_FILE_SYSTEM_TYPE, &BDEV_TEST_SUPER_OPERATIONS, |sb| {
        let root = sb.get_or_init_inode(1, || {
            VfsInode::new_dir_with_defaults(
                NodeFlags::empty(),
                VfsInodeInit::new(
                    1,
                    0,
                    Umode::new(NodeType::Directory, NodePermission::default()),
                ),
            )
        });
        Dentry::new_dir_from_inode(root, None, String::new())
    });
    let inode = sb.get_or_init_inode(2, || {
        VfsInode::new_special(
            Arc::new(SpecialOperations),
            NodeFlags::empty(),
            VfsInodeInit::new(2, 0, Umode::new(NodeType::Fifo, NodePermission::default())),
        )
    });
    let path = Path::new(
        crate::Mount::new_root(&sb),
        Dentry::new_file_from_inode(inode.clone(), None, String::new()),
    );
    let file = crate::dentry_open(
        path,
        crate::OpenFlags::READ_WRITE.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert!(!file.mode().contains(crate::FMode::WRITER));
    let past = ktime_types::SystemTime::from_unix_seconds(1);
    inode.set_accessed_at(past);
    inode.set_modified_at(past);
    inode.set_changed_at(past);
    assert_eq!(file.write(b"a"), Ok(1));
    let after_write = inode.times();
    assert!(after_write.modified_at.unix_seconds() > 1);
    assert!(after_write.changed_at.unix_seconds() > 1);
    let mut byte = [0];
    assert_eq!(file.read(&mut byte), Ok(1));
    assert_eq!(byte, *b"a");
    assert!(inode.times().accessed_at.unix_seconds() > 1);

    let before = inode.times();
    let gate = sb.umount_lock.write();
    let result = Arc::new(crate::Mutex::new(None));
    let saved = result.clone();
    let done = Arc::new(core::sync::atomic::AtomicBool::new(false));
    let finished = done.clone();
    let writer = ktask::spawn(move || {
        *saved.lock() = Some(file.write(b"x"));
        finished.store(true, Ordering::Release);
    });
    while !writer.state().is_blocked() && !done.load(Ordering::Acquire) {
        ktask::yield_now();
    }
    assert!(writer.state().is_blocked());
    sb.mark_read_only();
    drop(gate);
    writer.join();
    assert_eq!(result.lock().take().unwrap(), Ok(1));
    assert_eq!(inode.times().modified_at, before.modified_at);
    assert_eq!(inode.times().changed_at, before.changed_at);
}

struct WriteAdmissionOperations;

impl crate::InodeOperations for WriteAdmissionOperations {}

impl crate::FileOperations for WriteAdmissionOperations {
    fn supports_write(&self) -> bool {
        true
    }

    fn open(self: Arc<Self>, inode: &VfsInode, _file: &mut crate::VfsFileBuilder) -> VfsResult<()> {
        core::assert!(inode.super_block()?.umount_lock.try_write().is_none());
        Ok(())
    }

    fn write(&self, file: &crate::VfsFile, bytes: &[u8], _offset: u64) -> VfsResult<usize> {
        core::assert!(file.path().super_block().umount_lock.try_write().is_none());
        Ok(bytes.len())
    }
}

#[def_test]
fn old_writable_descriptors_reenter_lifecycle_admission_for_each_write() {
    let sb = crate::nullfs::new_superblock();
    let mount = crate::Mount::new_root(&sb);
    let operations = Arc::new(WriteAdmissionOperations);
    let inode = VfsInode::new_file_with_operations(
        Arc::new(()),
        operations.clone(),
        operations,
        NodeFlags::PRIVATE,
        VfsInodeInit::new(
            9,
            0,
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let dentry = Dentry::new_file_from_inode(inode, None, "write-admission".into());
    dentry.bind_super_block(&sb);
    let file = crate::dentry_open(
        Path::new(mount, dentry),
        crate::OpenFlags::READ_WRITE.bits(),
        kcred::initial_cred(),
    )
    .unwrap();
    assert!(sb.umount_lock.try_write().is_some());
    assert_eq!(file.write(b"one"), Ok(3));
    assert!(sb.umount_lock.try_write().is_some());
    assert_eq!(file.write(b"two"), Ok(3));
}

#[def_test]
fn mapping_write_admission_reports_contention_without_waiting_under_an_mm_lock() {
    let sb = crate::nullfs::new_superblock();
    let guard = sb.umount_lock.write();
    assert!(matches!(sb.try_start_write(), Err(VfsError::WouldBlock)));
    drop(guard);
    assert!(sb.try_start_write().is_ok());
}
