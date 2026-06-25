// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inode address-space and page-cache bridge for KFS.

use alloc::sync::Arc;

use kvfs::{Location, NodeFlags, VfsResult};
use memaddr::PAGE_SIZE_4K;
use pagecache::{Mapping, MappingKind};

fn is_in_memory(location: &Location) -> bool {
    location.flags().contains(NodeFlags::ALWAYS_CACHE)
}

/// Returns the Linux-style inode-owned page-cache mapping for a regular file.
pub(crate) fn mapping_for_location(location: &Location) -> VfsResult<Arc<Mapping>> {
    let in_memory = is_in_memory(location);
    let initial_len = location.len().unwrap_or(0);
    location.check_is_file()?;
    let address_space = location.address_space();
    let mapping = address_space.get_or_insert_page_cache(
        if in_memory {
            MappingKind::InMemory
        } else {
            MappingKind::FileBacked
        },
        initial_len,
    );
    mapping.set_len(initial_len)?;
    Ok(mapping)
}

pub(crate) fn set_len(location: &Location, len: u64) -> VfsResult<()> {
    location.entry().as_file()?.set_len(len)?;
    mapping_for_location(location)?.set_len(len)
}

pub(crate) fn flush_and_evict_from(location: &Location, offset: u64) -> VfsResult<()> {
    let mapping = mapping_for_location(location)?;
    location.address_space().writepages_from(offset, false)?;
    mapping.invalidate_from_page(offset / PAGE_SIZE_4K as u64)?;
    Ok(())
}

pub(crate) fn sync(location: &Location, data_only: bool) -> VfsResult<()> {
    let _mapping = mapping_for_location(location)?;
    location.address_space().writepages(data_only)
}
