// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Address-space resource admission under the owning MM lock.

use kerrno::{KError, KResult};
use memaddr::VirtAddrRange;
use memspace::MmSpace;

/// Checks the projected virtual footprint against a snapshotted RLIMIT_AS.
/// Replacement mappings are charged only for their net growth. Unlimited
/// limits keep the usual mapping path free of a VMA accounting walk.
pub(crate) fn check_address_space_limit(
    aspace: &MmSpace,
    added_bytes: usize,
    replacement: Option<VirtAddrRange>,
    limit_bytes: u64,
) -> KResult {
    if limit_bytes == u64::MAX {
        return Ok(());
    }
    let replaced = replacement.map_or(0, |range| {
        aspace
            .vmas()
            .map(|vma| {
                let start = vma.start().max(range.start);
                let end = vma.end().min(range.end);
                if end > start { (end - start) as u64 } else { 0 }
            })
            .sum::<u64>()
    });
    let projected = aspace
        .mapped_size()
        .checked_sub(replaced)
        .and_then(|size| size.checked_add(added_bytes as u64));
    if projected.is_some_and(|size| size <= limit_bytes) {
        Ok(())
    } else {
        Err(KError::NoMemory)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/limits.rs");
