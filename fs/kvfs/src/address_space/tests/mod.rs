// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared address-space test fixtures.

use alloc::sync::Weak;

use memaddr::PAGE_SIZE_4K;
use pagecache::Folio;
use unittest::def_test;

use super::{AddressSpace, AddressSpaceOperations, empty_address_space_operations};
use crate::{FileOperations, InodeOperations, VfsError, VfsResult};

mod read;
mod truncate;
mod writeback;

struct TruncateOps;

static TRUNCATE_OPERATIONS: TruncateOps = TruncateOps;

impl InodeOperations for TruncateOps {}
impl FileOperations for TruncateOps {}

impl AddressSpaceOperations for TruncateOps {
    fn set_len(&self, mapping: &AddressSpace, len: u64) -> VfsResult<()> {
        mapping.truncate_setsize(len)
    }

    fn read_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<usize> {
        folio.data().fill(0);
        Ok(folio.data().len())
    }
}

#[def_test]
fn address_space_identity_is_owned_by_address_space() {
    let first = AddressSpace::new(Weak::new(), empty_address_space_operations());
    let second = AddressSpace::new(Weak::new(), empty_address_space_operations());

    assert_ne!(first.object_id(), second.object_id());
}

impl AddressSpace {
    #[cfg(unittest)]
    fn write_cached_bytes(&self, offset: u64, src: &[u8]) -> VfsResult<usize> {
        let mut written = 0usize;
        while written < src.len() {
            let pos = offset
                .checked_add(written as u64)
                .ok_or(VfsError::InvalidInput)?;
            let index = pos / PAGE_SIZE_4K as u64;
            let page_off = (pos % PAGE_SIZE_4K as u64) as usize;
            let step = (src.len() - written).min(PAGE_SIZE_4K - page_off);
            self.with_folio_or_create(index, |folio| {
                folio.data()[page_off..page_off + step]
                    .copy_from_slice(&src[written..written + step]);
                folio.mark_dirty();
                Ok(())
            })?;
            written += step;
        }
        Ok(written)
    }
}
