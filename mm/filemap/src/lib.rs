// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate alloc;
#[macro_use]
extern crate log;

mod invalidate;
mod mmap;
mod private;
mod runtime;
mod shared;

#[cfg(unittest)]
mod test_support {
    use alloc::sync::Arc;

    use kvfs::{NodePermission, Path, VfsFile, dentry_open};
    use memfs::shmem;

    const O_RDWR: u32 = 2;

    pub(crate) fn anonymous_location(name: &str) -> Path {
        shmem::create_kernel_file(name, NodePermission::from_bits_truncate(0o600))
            .map(|file| file.into_path())
            .expect("create anonymous file")
    }

    pub(crate) fn open_test_file(location: Path, flags: u32) -> Arc<VfsFile> {
        dentry_open(location, flags).expect("open page-cache file")
    }

    pub(crate) fn page_cache_file(name: &str) -> Arc<VfsFile> {
        open_test_file(anonymous_location(name), O_RDWR)
    }
}

/// File-backed mapping mode.
///
/// This makes the Linux split explicit:
///
/// - `Shared`: VMA faults resolve directly against the underlying cached
///   content object (`address_space` / inode-owned mapping)
/// - `Private`: initial reads come from the same object, but writes materialize
///   private COW pages for the mapping instance
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileMappingMode {
    /// Linux `MAP_SHARED` / shared file-backed mapping.
    Shared,
    /// Linux `MAP_PRIVATE` / private file-backed mapping.
    Private,
}

pub use self::{
    mmap::{FileMmapRequest, mmap_private_file, mmap_shared_file},
    runtime::new_file_private_vma,
};
