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

unittest::path_mod_test!(test_support, "../ut-tests/test_support.rs");

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
