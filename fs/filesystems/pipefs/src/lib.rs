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
    get_next_ino,
    libfs::new_pseudo_super_block,
    metadata_only_inode_operations,
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
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Boot installs the permanent pseudo \
                      filesystem before any anonymous-file or pipe creation"
        )]
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
        metadata_only_inode_operations(),
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

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
