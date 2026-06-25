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

    use kfs::{File, OpenOptions};
    use kvfs::{Location, Mountpoint, NodePermission, NodeType, OpenOptions as VfsOpenOptions};
    use memfs::MemoryFs;

    pub(crate) fn anonymous_location(name: &str) -> Location {
        let fs = MemoryFs::new_with_name_and_flags("tmpfs", 0);
        let root = Location::new(Mountpoint::new_root(&fs), fs.root_dir());
        root.open_file(
            name,
            &VfsOpenOptions {
                create: true,
                create_new: true,
                node_type: NodeType::RegularFile,
                permission: NodePermission::from_bits_truncate(0o600),
                user: None,
            },
        )
        .expect("create anonymous file")
    }

    pub(crate) fn page_cache_file(name: &str) -> Arc<File> {
        Arc::new(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open_loc(anonymous_location(name))
                .expect("open page-cache file"),
        )
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
