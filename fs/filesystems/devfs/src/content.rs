// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Memory-backed regular files for devtmpfs directories.
//!
//! Linux devtmpfs is ramfs-backed, so its directories accept plain file
//! creation. POSIX shared memory depends on that behaviour: after an init
//! grafts a fresh devtmpfs mount over `/dev`, the boot-time `/dev/shm` tmpfs
//! is shadowed and `sem_open` creates its semaphore files directly in the
//! devtmpfs `/dev/shm` directory.

use alloc::sync::Arc;
use core::ops::ControlFlow;

use iov_iter::{IovIterDest, IovIterSource};
use kvfs::{
    AddressSpace, AddressSpaceOperations, FileOperations, InodeOperations, Kiocb, Metadata,
    MetadataUpdate, MountIdmap, NodeFlags, NodeType, SimpleFs, SimpleFsNode, Umode, VfsError,
    VfsFile, VfsInode, VfsResult, WriteBeginRequest, WriteEndRequest, WritebackControl,
    libfs::simple_write_end,
};
use pagecache::FolioRead;

static ZERO_PAGE: [u8; memaddr::PAGE_SIZE_4K] = [0; memaddr::PAGE_SIZE_4K];

/// A plain memory-backed regular file created inside a devtmpfs directory.
pub(crate) struct ContentFile {
    node: SimpleFsNode,
}

impl ContentFile {
    /// Creates a regular-file inode owned by the devtmpfs superblock.
    ///
    /// `mode` carries the final permission bits (umask and `inode_init_owner`
    /// have already been applied by the VFS create path).
    pub(crate) fn new_inode(fs: Arc<SimpleFs>, mode: Umode, uid: u32, gid: u32) -> Arc<VfsInode> {
        let file = Arc::new(Self {
            node: SimpleFsNode::new_with_owner(
                fs,
                NodeType::RegularFile,
                mode.permission(),
                uid,
                gid,
            ),
        });
        let init = file.node.inode_init();
        VfsInode::new_file_with_address_space_and_flags(
            file,
            NodeFlags::ALWAYS_CACHE,
            &DEVFS_CONTENT_SPACE_OPERATIONS,
            init,
        )
    }
}

impl InodeOperations for ContentFile {
    fn getattr(
        &self,
        idmap: &MountIdmap,
        path: Option<&kvfs::Path>,
        request_mask: kvfs::GetattrRequestMask,
        query_flags: kvfs::GetattrQueryFlags,
    ) -> VfsResult<Metadata> {
        self.node.getattr(idmap, path, request_mask, query_flags)
    }

    fn setattr(
        &self,
        idmap: &MountIdmap,
        dentry: &kvfs::Dentry,
        update: MetadataUpdate,
    ) -> VfsResult<MetadataUpdate> {
        self.node.setattr(idmap, dentry, update)
    }
}

impl FileOperations for ContentFile {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterDest<'_>) -> VfsResult<usize> {
        let inode = iocb.file().inode();
        let mapping = iocb.file().mapping();
        let mut offset = iocb.ki_pos();
        let mut total = 0;
        while iter.count() != 0 {
            let file_len = inode.size();
            if offset >= file_len {
                break;
            }
            let page_offset = (offset % memaddr::PAGE_SIZE_4K as u64) as usize;
            let count = (file_len - offset)
                .min(iter.count() as u64)
                .saturating_add(page_offset as u64)
                .div_ceil(memaddr::PAGE_SIZE_4K as u64)
                .min(pagecache::FolioBatch::CAPACITY as u64) as usize;
            let mut is_done = false;
            let result = mapping.visit_resident_folios(
                offset / memaddr::PAGE_SIZE_4K as u64,
                count,
                |index, folio| {
                    debug_assert_eq!(index, offset / memaddr::PAGE_SIZE_4K as u64);
                    let page_offset = (offset % memaddr::PAGE_SIZE_4K as u64) as usize;
                    let step = inode
                        .size()
                        .saturating_sub(offset)
                        .min((memaddr::PAGE_SIZE_4K - page_offset) as u64)
                        as usize;
                    let copied = match folio {
                        Some(folio) => {
                            iter.copy_to_iter(&folio.data()[page_offset..page_offset + step])?
                        }
                        None => iter.copy_to_iter(&ZERO_PAGE[..step])?,
                    };
                    offset += copied as u64;
                    total += copied;
                    is_done = copied < memaddr::PAGE_SIZE_4K - page_offset || iter.count() == 0;
                    Ok(if is_done {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    })
                },
            );
            match result {
                Err(error) if total == 0 => return Err(error),
                Err(_) => break,
                Ok(_) if is_done => break,
                Ok(_) => {}
            }
        }
        iocb.advance(total);
        Ok(total)
    }

    fn write_iter(&self, iocb: &mut Kiocb<'_>, iter: &mut IovIterSource<'_>) -> VfsResult<usize> {
        iocb.generic_file_write_iter(iter)
    }

    fn fsync(
        &self,
        _file: &VfsFile,
        _range: core::ops::Range<u64>,
        _data_only: bool,
    ) -> VfsResult<()> {
        // Memory-backed content has no writeback target.
        Ok(())
    }
}

static DEVFS_CONTENT_SPACE_OPERATIONS: DevfsContentSpaceOperations = DevfsContentSpaceOperations;

struct DevfsContentSpaceOperations;

impl AddressSpaceOperations for DevfsContentSpaceOperations {
    fn read_folio(&self, _mapping: &AddressSpace, mut folio: FolioRead) -> VfsResult<()> {
        let data = folio.data();
        data.fill(0);
        folio.complete(Ok(()))
    }

    fn writepages(
        &self,
        _mapping: &AddressSpace,
        _control: &mut WritebackControl,
    ) -> VfsResult<()> {
        Ok(())
    }

    fn write_begin(&self, _mapping: &AddressSpace, _request: WriteBeginRequest) -> VfsResult<()> {
        Ok(())
    }

    fn write_end(&self, mapping: &AddressSpace, request: WriteEndRequest) -> VfsResult<usize> {
        let copied = simple_write_end(mapping, request)?;
        if copied != 0 {
            let inode = mapping.inode().ok_or(VfsError::InvalidInput)?;
            let file = inode.private::<ContentFile>()?;
            file.node.set_size(inode.size());
        }
        Ok(copied)
    }
}
