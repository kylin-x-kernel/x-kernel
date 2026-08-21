// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Anonymous pipe pseudo filesystem.

#![no_std]

extern crate alloc;

use alloc::{format, string::String, sync::Arc};

use kcred::Cred;
use klazy::Once;
use kvfs::{
    Dentry, DentryOperations, FMode, FileOperations, FileSystemType, Mount, NodeFlags,
    NodePermission, NodeType, OpenFlags, Umode, VfsFile, VfsInode, VfsInodeInit, VfsResult,
    empty_inode_operations, get_next_ino,
    libfs::new_pseudo_super_block,
    pipe::{PipeObject, pipe_file_operations},
};
use memaddr::PAGE_SIZE_4K;

static PIPE_FS: Once<PipeFs> = Once::new();
static PIPE_FS_TYPE: FileSystemType = FileSystemType::internal("pipefs");

const PIPE_FS_MAGIC: u32 = 0x5049_5045;

/// Hidden pseudo filesystem that owns anonymous pipe inodes.
struct PipeFs {
    mount: Arc<Mount>,
}

impl PipeFs {
    fn new() -> Self {
        let super_block =
            new_pseudo_super_block(&PIPE_FS_TYPE, PIPE_FS_MAGIC, &PIPE_DENTRY_OPERATIONS);
        Self {
            mount: Mount::new_root(&super_block),
        }
    }

    fn global() -> &'static Self {
        PIPE_FS
            .get()
            .expect("pipe filesystem must be initialized before use")
    }
}

/// Initializes the hidden pipe filesystem during boot.
pub fn init_pipefs() {
    let _ = PIPE_FS.call_once(PipeFs::new);
}

/// Creates the read and write open files for an anonymous pipe.
///
/// Only [`OpenFlags::NONBLOCK`] is retained from `flags`; pipefs derives the
/// fixed read-only and write-only access modes. Both file views capture the
/// same `cred` as their open credential.
pub fn create_pipe_files(
    flags: OpenFlags,
    cred: Arc<Cred>,
) -> VfsResult<(Arc<VfsFile>, Arc<VfsFile>)> {
    let status_flags = flags & OpenFlags::NONBLOCK;
    let pipe = PipeObject::new_anonymous();
    let operations = pipe_file_operations();
    let inode = new_pipe_inode(pipe.clone(), &cred, operations.clone());
    let write_file = PipeFs::global().mount.alloc_file_pseudo(
        inode,
        "",
        FMode::WRITE | FMode::STREAM,
        OpenFlags::WRITE_ONLY | status_flags,
        operations.clone(),
        cred,
    )?;
    write_file.set_private_data(pipe.clone());
    let read_file = write_file.alloc_clone_with_private_data(
        FMode::READ | FMode::STREAM,
        status_flags,
        operations,
        pipe,
    )?;
    Ok((read_file, write_file))
}

fn new_pipe_inode(
    pipe: Arc<PipeObject>,
    cred: &Cred,
    file_operations: Arc<dyn FileOperations>,
) -> Arc<VfsInode> {
    let timestamp = ktime::realtime();
    let init = VfsInodeInit::new(
        get_next_ino(),
        0,
        Umode::new(
            NodeType::Fifo,
            NodePermission::OWNER_READ | NodePermission::OWNER_WRITE,
        ),
    )
    .with_owner_links_and_rdev(cred.fsuid(), cred.fsgid(), 1, Default::default())
    .with_stat_data(PAGE_SIZE_4K as u64, 0, timestamp, timestamp, timestamp);
    VfsInode::new_file_with_operations(
        pipe,
        empty_inode_operations(),
        file_operations,
        NodeFlags::PRIVATE,
        init,
    )
}

struct PipeDentryOperations;

static PIPE_DENTRY_OPERATIONS: PipeDentryOperations = PipeDentryOperations;

impl DentryOperations for PipeDentryOperations {
    fn d_dname(&self, dentry: &Dentry) -> VfsResult<Option<String>> {
        Ok(Some(format!("pipe:[{}]", dentry.inode())))
    }
}

#[cfg(unittest)]
mod tests {
    use kerrno::KError;
    use kpoll::IoEvents;
    use unittest::def_test;

    use super::*;

    fn pipe_files() -> (Arc<VfsFile>, Arc<VfsFile>, Arc<PipeObject>) {
        init_pipefs();
        let (read_file, write_file) = create_pipe_files(OpenFlags::empty(), kcred::initial_cred())
            .expect("anonymous pipe files open");
        let pipe = PipeObject::from_file(&read_file).expect("pipe state is installed");
        (read_file, write_file, pipe)
    }

    #[def_test]
    fn anonymous_pipe_files_share_inode_owned_state() {
        let (read_file, write_file, pipe) = pipe_files();
        let write_pipe = PipeObject::from_file(&write_file).expect("pipe state is installed");
        let inode_pipe = read_file
            .path()
            .inode()
            .downcast::<PipeObject>()
            .expect("pipe inode owns pipe state");

        assert!(read_file.mode().contains(FMode::READ));
        assert!(write_file.mode().contains(FMode::WRITE));
        assert!(Arc::ptr_eq(&pipe, &write_pipe));
        assert!(Arc::ptr_eq(&pipe, &inode_pipe));
        assert_eq!(pipe.capacity(), 64 * 1024);
        assert!(
            read_file
                .path()
                .display_path()
                .unwrap()
                .starts_with("pipe:[")
        );
    }

    #[def_test]
    fn distinct_anonymous_pipes_have_distinct_inodes() {
        let (first_read, first_write, _) = pipe_files();
        let (second_read, ..) = pipe_files();

        assert_eq!(
            first_read.path().inode().inode(),
            first_write.path().inode().inode()
        );
        assert_ne!(
            first_read.path().inode().inode(),
            second_read.path().inode().inode()
        );
    }

    #[def_test]
    fn anonymous_pipe_derives_endpoint_access_flags() {
        init_pipefs();
        let (read_file, write_file) = create_pipe_files(
            OpenFlags::APPEND | OpenFlags::NONBLOCK,
            kcred::initial_cred(),
        )
        .expect("anonymous pipe files open");

        assert_eq!(read_file.flags(), OpenFlags::NONBLOCK);
        assert_eq!(
            write_file.flags(),
            OpenFlags::WRITE_ONLY | OpenFlags::NONBLOCK
        );
    }

    #[def_test]
    fn anonymous_pipe_poll_after_writer_drop() {
        let (read_file, write_file, _) = pipe_files();
        drop(write_file);

        let events = read_file.poll();
        assert!(!events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));
    }

    #[def_test]
    fn anonymous_pipe_buffered_data_survives_writer_drop() {
        let (read_file, write_file, _) = pipe_files();
        let data = b"hello";
        assert_eq!(write_file.write(data).unwrap(), data.len());
        drop(write_file);

        let events = read_file.poll();
        assert!(events.contains(IoEvents::IN));
        assert!(events.contains(IoEvents::HUP));

        let mut buf = [0u8; 5];
        assert_eq!(read_file.read(&mut buf).unwrap(), data.len());
        assert_eq!(&buf, data);
    }

    #[def_test]
    fn anonymous_pipe_resize_rounds_to_power_of_two_pages() {
        let (_, _, pipe) = pipe_files();

        pipe.resize(5000).unwrap();
        assert_eq!(pipe.capacity(), 8192);
        pipe.resize(12 * 1024).unwrap();
        assert_eq!(pipe.capacity(), 16 * 1024);
    }

    #[def_test]
    fn anonymous_pipe_resize_rejects_invalid_sizes() {
        let (_, _, pipe) = pipe_files();

        assert_eq!(
            pipe.resize(1024 * 1024 + 1),
            Err(KError::OperationNotPermitted)
        );
        assert_eq!(pipe.capacity(), 64 * 1024);
        assert_eq!(pipe.resize(usize::MAX), Err(KError::InvalidInput));
        assert_eq!(pipe.capacity(), 64 * 1024);
    }

    #[def_test]
    fn anonymous_pipe_nonblocking_small_write_is_atomic() {
        let (_read_file, write_file, pipe) = pipe_files();
        pipe.resize(PAGE_SIZE_4K).unwrap();
        write_file.set_nonblocking(true);

        let fill = [0u8; PAGE_SIZE_4K - 64];
        assert_eq!(write_file.write(&fill).unwrap(), fill.len());

        let payload = [1u8; 128];
        assert_eq!(write_file.write(&payload), Err(KError::WouldBlock));
    }
}
