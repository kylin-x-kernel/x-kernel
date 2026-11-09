// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Bounded data holds awaiting the page-table guard's TLB completion.

use alloc::vec::Vec;

use arrayvec::ArrayVec;

const LOCAL_RETIRED_REFERENCES: usize = 16;
const MAX_RETIRED_REFERENCES: usize = 256;

// Like mmu_gather's local bundle and bounded extra batches, keep small
// operations allocation-free and make allocation failure a drain boundary.
pub(super) struct RetiredReferences<T> {
    local: ArrayVec<T, LOCAL_RETIRED_REFERENCES>,
    overflow: Vec<T>,
    #[cfg(unittest)]
    pub(super) should_fail_reservation: bool,
}

impl<T> RetiredReferences<T> {
    pub(super) const fn new() -> Self {
        Self {
            local: ArrayVec::new_const(),
            overflow: Vec::new(),
            #[cfg(unittest)]
            should_fail_reservation: false,
        }
    }

    #[cfg(feature = "copy-from")]
    pub(super) fn is_empty(&self) -> bool {
        self.local.is_empty() && self.overflow.is_empty()
    }

    pub(super) fn is_full(&self) -> bool {
        self.local.len() + self.overflow.len() == MAX_RETIRED_REFERENCES
    }

    pub(super) fn try_push(&mut self, reference: T) -> Result<(), T> {
        if !self.local.is_full() {
            self.local.push(reference);
            return Ok(());
        }
        if self.is_full() {
            return Err(reference);
        }
        if self.overflow.capacity() == 0 {
            #[cfg(unittest)]
            if self.should_fail_reservation {
                return Err(reference);
            }
            if self
                .overflow
                .try_reserve_exact(MAX_RETIRED_REFERENCES - LOCAL_RETIRED_REFERENCES)
                .is_err()
            {
                return Err(reference);
            }
        }
        self.overflow.push(reference);
        Ok(())
    }

    pub(super) fn forget_holds(&mut self) {
        // An unconfirmed shootdown cannot authorize reuse. Keep lifetime counts
        // pinned rather than running their destructors after the guard ends.
        for reference in self.local.drain(..) {
            core::mem::forget(reference);
        }
        for reference in self.overflow.drain(..) {
            core::mem::forget(reference);
        }
    }

    pub(super) fn clear(&mut self) {
        self.local.clear();
        self.overflow.clear();
    }
}
