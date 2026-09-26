// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffered read boundaries and page removal during window consumption.

use alloc::{sync::Weak, vec, vec::Vec};

use iov_iter::{IovSink, iov_iter_dest, iov_iter_kvec_dest};
use kerrno::{KError, KResult};
use memaddr::PAGE_SIZE_4K;
use pagecache::Folio;
use unittest::def_test;

use super::AddressSpace;
use crate::{AddressSpaceOperations, VfsResult};

struct ReadOps;
static READ_OPS: ReadOps = ReadOps;

impl AddressSpaceOperations for ReadOps {
    fn read_folio(&self, _mapping: &AddressSpace, folio: &mut Folio) -> VfsResult<usize> {
        let index = folio.index();
        folio.data().fill((index % 251) as u8);
        Ok(PAGE_SIZE_4K)
    }
}

#[def_test]
fn buffered_read_crosses_windows_with_unaligned_eof_and_cache_holes() {
    let mapping = AddressSpace::new(Weak::new(), &READ_OPS);
    let file_len = 70 * PAGE_SIZE_4K + 17;
    for index in (0..70).step_by(2) {
        mapping
            .with_folio_or_create(index, |_| Ok(()))
            .expect("warm alternating pages");
    }
    let offset = 37;
    let mut output = vec![0xff; file_len];
    let mut iter = iov_iter_kvec_dest(&mut output);
    let copied = mapping
        .read_from_page_cache(&mut iter, offset, file_len as u64)
        .expect("read across multiple windows");
    assert_eq!(copied, file_len - offset as usize);
    for (position, byte) in output[..copied].iter().enumerate() {
        assert_eq!(
            *byte,
            (((position + offset as usize) / PAGE_SIZE_4K) % 251) as u8
        );
    }
    assert!(output[copied..].iter().all(|byte| *byte == 0xff));
}

struct ShortSink {
    copied: usize,
    limit: usize,
}

impl IovSink for ShortSink {
    fn count(&self) -> usize {
        PAGE_SIZE_4K * 40 - self.copied
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        let copied = src.len().min(self.limit);
        self.copied += copied;
        Ok(copied)
    }

    fn revert(&mut self, _count: usize) -> KResult<()> {
        Err(KError::InvalidInput)
    }
}

#[def_test]
fn buffered_read_stops_on_short_or_zero_copy() {
    for limit in [0, 23] {
        let mapping = AddressSpace::new(Weak::new(), &READ_OPS);
        let mut sink = ShortSink { copied: 0, limit };
        let mut iter = iov_iter_dest(&mut sink);
        assert_eq!(
            mapping.read_from_page_cache(&mut iter, 0, (PAGE_SIZE_4K * 40) as u64),
            Ok(limit)
        );
        assert_eq!(sink.copied, limit);
    }
}

struct InvalidatingSink<'a> {
    mapping: &'a AddressSpace,
    output: Vec<u8>,
}

impl IovSink for InvalidatingSink<'_> {
    fn count(&self) -> usize {
        PAGE_SIZE_4K * 3 - self.output.len()
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        self.output.extend_from_slice(src);
        if self.output.len() == PAGE_SIZE_4K {
            // Remove only future pages: the current folio is still locked by
            // the copy callback. This models residency changing after prepare.
            self.mapping
                .i_pages
                .resize_cached_folios((PAGE_SIZE_4K * 3) as u64, PAGE_SIZE_4K as u64);
        }
        Ok(src.len())
    }

    fn revert(&mut self, _count: usize) -> KResult<()> {
        Err(KError::InvalidInput)
    }
}

#[def_test]
fn buffered_read_resolves_pages_removed_after_window_preparation() {
    let mapping = AddressSpace::new(Weak::new(), &READ_OPS);
    for index in 0..3 {
        mapping
            .with_folio_or_create(index, |folio| {
                folio.data().fill(0x99);
                Ok(())
            })
            .expect("populate resident pages");
    }
    let mut sink = InvalidatingSink {
        mapping: &mapping,
        output: Vec::new(),
    };
    let mut iter = iov_iter_dest(&mut sink);
    assert_eq!(
        mapping.read_from_page_cache(&mut iter, 0, (PAGE_SIZE_4K * 3) as u64),
        Ok(PAGE_SIZE_4K * 3)
    );
    assert!(sink.output[..PAGE_SIZE_4K].iter().all(|byte| *byte == 0x99));
    assert!(
        sink.output[PAGE_SIZE_4K..PAGE_SIZE_4K * 2]
            .iter()
            .all(|byte| *byte == 1)
    );
    assert!(
        sink.output[PAGE_SIZE_4K * 2..]
            .iter()
            .all(|byte| *byte == 2)
    );
}
