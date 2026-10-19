// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Page-cache storage and algorithms.
//!
//! [`MappingPages`] is an implementation component owned by the VFS inode
//! address space. It is not a second Linux `address_space` analogue and must
//! not be held directly by VMA or open-file runtimes.
#![no_std]

extern crate alloc;

mod filemap;
mod folio;
mod folio_batch;
mod mapping_pages;
mod readahead;
mod truncate;
mod writeback;

pub use folio::Folio;
pub use folio_batch::FolioBatch;
pub use mapping_pages::{CacheResidency, MappingPages};
pub use writeback::WritebackRangeResult;

/// Page index within a mapping.
pub type PageIndex = u64;

unittest::path_mod_test!(pagecache_unittest, "../ut-tests/pagecache_unittest.rs");
