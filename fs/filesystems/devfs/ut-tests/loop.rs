// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn unbound_loop_completes_flush_bios() {
    let disk = block::Gendisk::alloc_mq(
        alloc::string::String::from("loop-bio-test"),
        0,
        0,
        1,
        Arc::new(LoopDevice::new(0)),
    )
    .unwrap();
    let device = disk.part0();
    let mut waits = Vec::new();
    for _ in 0..2 {
        let (bio, wait) = block::completion::BioWait::prepare(
            device.clone(),
            0,
            block::BioOperation::Flush,
            Vec::new(),
        )
        .unwrap_or_else(|(error, _)| panic!("prepare: {error:?}"));
        device.submit_bio(bio);
        waits.push(wait);
    }
    for wait in waits {
        assert!(wait.wait().0.is_ok());
    }
}

#[unittest::def_test]
fn ioctl_permissions_follow_command_and_open_mode() {
    use block::BlockOpenMode as Mode;
    use linux_raw_sys::ioctl::{BLKFRASET, BLKRASET};

    let disk = block::Gendisk::alloc_mq(
        "test-loop-ioctl".into(),
        7,
        0,
        1,
        Arc::new(LoopDevice::new(0)),
    )
    .expect("valid unpublished loop disk");
    let device = disk.part0();
    for (cmd, mode, has_sys_admin, error) in [
        (
            LOOP_SET_STATUS,
            Mode::READ,
            false,
            KError::from(LinuxError::EPERM),
        ),
        (LOOP_SET_STATUS, Mode::WRITE, false, KError::BadAddress),
        (LOOP_SET_STATUS, Mode::READ, true, KError::BadAddress),
        (LOOP_SET_FD, Mode::READ, false, KError::BadFileDescriptor),
        (
            LOOP_CLR_FD,
            Mode::READ,
            false,
            KError::from(LinuxError::ENXIO),
        ),
        (BLKRASET, Mode::WRITE, false, KError::PermissionDenied),
        (BLKFRASET, Mode::WRITE, false, KError::PermissionDenied),
    ] {
        assert_eq!(
            device.as_ref().ioctl(
                mode,
                cmd,
                usize::MAX,
                has_sys_admin,
                || panic!("these commands do not copy a generic int"),
                |_| panic!("these commands do not copy generic output"),
            ),
            Err(error)
        );
    }
}

#[unittest::def_test]
fn loop_passes_one_scatter_iterator_and_returns_short_io_memory() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kvfs::{FileOperations, Kiocb};
    struct Backing {
        calls: AtomicUsize,
    }
    impl FileOperations for Backing {
        fn supports_read(&self) -> bool {
            true
        }

        fn supports_write(&self) -> bool {
            true
        }

        fn read_iter(
            &self,
            iocb: &mut Kiocb<'_>,
            iter: &mut iov_iter::IovIterDest<'_>,
        ) -> VfsResult<usize> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(iocb.ki_pos(), 512);
            assert_eq!(iter.count(), 512);
            // Cross an extent boundary, rewind across it, then produce a short read.
            assert_eq!(iter.copy_to_iter(&[0x5a; 20])?, 20);
            iter.revert(5)?;
            assert_eq!(iter.copy_to_iter(&[0x6b; 16])?, 16);
            iocb.advance(31);
            Ok(31)
        }

        fn write_iter(
            &self,
            iocb: &mut Kiocb<'_>,
            iter: &mut iov_iter::IovIterSource<'_>,
        ) -> VfsResult<usize> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            assert_eq!(iocb.ki_pos(), 512);
            assert_eq!(iter.count(), 512);
            let mut data = [0; 512];
            assert_eq!(iter.copy_from_iter(&mut data)?, 512);
            assert_eq!(data, [0x37; 512]);
            iter.revert(512)?;
            assert_eq!(iter.copy_from_iter(&mut data)?, 512);
            iocb.advance(512);
            Ok(512)
        }
    }
    struct InodeOps;
    impl kvfs::InodeOperations for InodeOps {}
    let super_block = kvfs::SimpleFs::new_with(&crate::FILE_SYSTEM_TYPE, 0, |fs| {
        kvfs::SimpleDir::new_maker(fs, Arc::new(kvfs::DirMapping::new()))
    });
    let operations = Arc::new(Backing {
        calls: AtomicUsize::new(0),
    });
    let inode = kvfs::VfsInode::new_file_with_operations(
        Arc::new(()),
        Arc::new(InodeOps),
        operations.clone(),
        kvfs::NodeFlags::PRIVATE,
        kvfs::VfsInodeInit::new(
            2,
            1024,
            kvfs::Umode::new(NodeType::RegularFile, kvfs::NodePermission::default()),
        ),
    );
    let dentry = kvfs::Dentry::new_file_from_inode(
        inode,
        Some(super_block.root_dir()),
        "loop-backing".into(),
    );
    let file = kvfs::dentry_open(
        kvfs::Path::new(kvfs::Mount::new_root(&super_block), dentry),
        linux_raw_sys::general::O_RDWR,
        kcred::initial_cred(),
    )
    .unwrap();
    let backend = Arc::new(LoopDevice::new(0));
    *backend.file.lock() = Some(file);
    let device = block::Gendisk::alloc_mq("loop-scatter".into(), 0, 0, 1, backend)
        .unwrap()
        .part0();
    for operation in [block::BioOperation::Read, block::BioOperation::Write] {
        let vectors = [17, 495]
            .into_iter()
            .map(|len| block::BioVec::new(alloc::vec![0x37; len].into_boxed_slice()))
            .collect();
        let (status, vectors) = device.submit_bio_wait(1, operation, vectors);
        if operation == block::BioOperation::Read {
            assert_eq!(status, Err(block::DriverError::Io));
            let bytes: Vec<_> = vectors
                .iter()
                .flat_map(|v| v.bytes().iter().copied())
                .collect();
            assert_eq!(&bytes[..15], &[0x5a; 15]);
            assert_eq!(&bytes[15..31], &[0x6b; 16]);
            assert!(bytes[31..].iter().all(|byte| *byte == 0x37));
        } else {
            assert_eq!(status, Ok(()));
        }
    }
    assert_eq!(operations.calls.load(Ordering::Relaxed), 2);
}
