// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE};
use unittest::def_test;

use super::{
    MADV_DONTNEED, MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED, MadviseRequest,
    MremapFlags,
};

#[def_test]
fn mremap_dontunmap_requires_equal_sizes() {
    let flags = MremapFlags::from_raw(MREMAP_MAYMOVE | MREMAP_DONTUNMAP).unwrap();

    assert!(flags.validate_args(0x2000, 0x3000).is_err());
    assert!(flags.validate_args(0x2000, 0x2000).is_ok());
}

#[def_test]
fn mremap_fixed_and_dontunmap_require_maymove() {
    assert!(
        MremapFlags::from_raw(MREMAP_FIXED)
            .unwrap()
            .validate_args(0x2000, 0x2000)
            .is_err()
    );
    assert!(
        MremapFlags::from_raw(MREMAP_DONTUNMAP)
            .unwrap()
            .validate_args(0x2000, 0x2000)
            .is_err()
    );
}

/// Linux `do_madvise()` validates the range before acting on the advice, so an
/// unaligned address or an overflowing range is rejected even for the pure
/// performance hints that this kernel honours as no-ops.
#[def_test]
fn madvise_validates_the_range_before_the_advice() {
    let unaligned = 0x1000 + 1;
    // Page-aligned, and `addr + 0x2000` still overflows.
    let overflowing = usize::MAX - 0xFFF;
    // Page-aligned; `checked_add` survives, but rounding the end up to a
    // page boundary wraps.
    let rounding_overflows = 0xFFFF_FFFF_FFFF_F000;

    for advice in [MADV_NORMAL, MADV_RANDOM, MADV_WILLNEED].map(|advice| advice as i32) {
        assert!(MadviseRequest::from_raw(unaligned, 0x1000, advice).is_err());
        assert!(MadviseRequest::from_raw(overflowing, 0x2000, advice).is_err());
        assert!(
            MadviseRequest::from_raw(rounding_overflows, 0xFFF, advice).is_err(),
            "end rounding past the address space must be EINVAL"
        );
    }

    // Well-formed requests carry the range; only DONTNEED discards, the
    // hints still need a fully mapped range (`ENOMEM` at the aspace layer).
    for advice in [MADV_NORMAL, MADV_RANDOM, MADV_SEQUENTIAL, MADV_WILLNEED] {
        let request = MadviseRequest::from_raw(0x2000, 0x1000, advice as i32).unwrap();
        assert!(request.is_some());
        assert!(!request.unwrap().is_discard);
    }
    assert!(matches!(
        MadviseRequest::from_raw(0x2000, 0x1000, MADV_DONTNEED as i32),
        Ok(Some(_))
    ));
}
