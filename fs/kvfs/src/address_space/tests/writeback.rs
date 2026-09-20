// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Partial writeback completion and retry tests.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use pagecache::{Folio, PageIndex, WritebackRangeOutcome};
use unittest::def_test;

use super::{
    super::{AddressSpace, AddressSpaceOperations, WritebackControl},
    TruncateOps,
};
use crate::{
    NodeFlags, NodePermission, NodeType, Umode, VfsError, VfsInode, VfsInodeInit, VfsResult,
};

struct PrefixRetryState {
    fail_once: bool,
    calls: usize,
    durable: Vec<u8>,
}

struct PrefixRetryWritebackOps {
    state: Mutex<PrefixRetryState>,
}

impl AddressSpaceOperations for PrefixRetryWritebackOps {
    fn read_folio(
        &self,
        _mapping: &AddressSpace,
        folio: &mut Folio,
        _index: PageIndex,
    ) -> VfsResult<usize> {
        folio.data().fill(0);
        Ok(folio.data().len())
    }

    fn writepages(&self, mapping: &AddressSpace, control: &mut WritebackControl) -> VfsResult<()> {
        mapping.writeback_cached_ranges(control, PAGE_SIZE_4K * 2, |offset, data| {
            let start = usize::try_from(offset).map_err(|_| VfsError::InvalidInput)?;
            let end = start
                .checked_add(data.len())
                .ok_or(VfsError::InvalidInput)?;
            let mut state = self.state.lock();
            if state.durable.len() < end {
                state.durable.resize(end, 0);
            }
            state.calls += 1;
            if state.fail_once {
                let prefix = data.len() / 2;
                state.durable[start..start + prefix].copy_from_slice(&data[..prefix]);
                state.fail_once = false;
                return Ok(WritebackRangeOutcome::failed(
                    prefix,
                    VfsError::InvalidInput,
                ));
            }
            state.durable[start..end].copy_from_slice(data);
            Ok(WritebackRangeOutcome::complete(data.len()))
        })
    }
}

#[def_test]
fn failed_prefix_writeback_redirties_and_retry_converges() {
    let operations: &'static PrefixRetryWritebackOps =
        Box::leak(Box::new(PrefixRetryWritebackOps {
            state: Mutex::new(PrefixRetryState {
                fail_once: true,
                calls: 0,
                durable: Vec::new(),
            }),
        }));
    let input = vec![0x6d; PAGE_SIZE_4K * 2];
    let inode = VfsInode::new_file_with_address_space_and_flags(
        Arc::new(TruncateOps),
        NodeFlags::ALWAYS_CACHE,
        operations,
        VfsInodeInit::new(
            3,
            u64::try_from(input.len()).unwrap(),
            Umode::new(NodeType::RegularFile, NodePermission::default()),
        ),
    );
    let address_space = inode.address_space();
    address_space
        .write_cached_bytes(0, &input)
        .expect("populate dirty page-cache batch");

    let mut control = WritebackControl::all(false).with_nr_to_write(2);
    assert_eq!(
        address_space.writepages_control(&mut control),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(control.nr_to_write(), 1);
    for (index, expected_dirty) in [(0, false), (1, true)] {
        let state = address_space.page_cache.with_folio(index, |folio| {
            let folio = folio.expect("failed batch folio remains cached");
            (folio.is_dirty(), folio.is_under_writeback())
        });
        assert_eq!(state, (expected_dirty, false));
    }
    {
        let state = operations.state.lock();
        assert_eq!(state.calls, 1);
        assert_eq!(&state.durable[..PAGE_SIZE_4K], &input[..PAGE_SIZE_4K]);
        assert!(state.durable[PAGE_SIZE_4K..].iter().all(|byte| *byte == 0));
    }

    address_space
        .writepages_control(&mut control)
        .expect("redirtied batch converges on retry");
    assert_eq!(control.nr_to_write(), 0);
    for index in 0..2 {
        let state = address_space.page_cache.with_folio(index, |folio| {
            let folio = folio.expect("retried batch folio remains cached");
            (folio.is_dirty(), folio.is_under_writeback())
        });
        assert_eq!(state, (false, false));
    }
    let state = operations.state.lock();
    assert_eq!(state.calls, 2);
    assert_eq!(state.durable, input);
}
