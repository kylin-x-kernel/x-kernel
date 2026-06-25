// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use khal::paging::PageSize;
use memaddr::VirtAddr;
use memspace::InvalidateHandle;
use vmobj::{MappingViewNotifier, ObjectInvalidateWork, ObjectViewHit};

/// Reverse-mapping notifier for one file-backed VMA view.
pub struct MmSpaceInvalidate {
    handle: InvalidateHandle,
}

impl MmSpaceInvalidate {
    pub fn new(handle: InvalidateHandle) -> Arc<Self> {
        Arc::new(Self { handle })
    }
}

impl MappingViewNotifier for MmSpaceInvalidate {
    fn invalidate(&self, work: &ObjectInvalidateWork, hit: &ObjectViewHit) {
        let Some(aligned_hit) = hit.aligned_object_suffix(PageSize::Size4K as u64) else {
            return;
        };

        let unmap_start = VirtAddr::from_usize(aligned_hit.vma_start() as usize);
        let unmap_len = aligned_hit.vma_len();
        let Some(request) = work.request_for_subhit(hit, &aligned_hit) else {
            warn!(
                "Ignored file-backed hit {} not carried by its invalidate work",
                hit.view().id().raw()
            );
            return;
        };
        match self.handle.submit(request) {
            Ok(_) => {}
            Err(err) => {
                warn!(
                    "Failed to invalidate mapping view {} at {:?} (len={}): {err}",
                    hit.view().id().raw(),
                    unmap_start,
                    unmap_len
                );
            }
        }
    }
}
