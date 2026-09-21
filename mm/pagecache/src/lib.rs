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
mod mapping_pages;
mod readahead;
mod truncate;
mod writeback;

pub use folio::Folio;
pub use mapping_pages::{MappingPages, PageCacheKind};
pub use writeback::{WritebackPassOutcome, WritebackRangeOutcome, WritebackStats};

/// Page index within a mapping.
pub type PageIndex = u64;

#[cfg(unittest)]
mod tests;
