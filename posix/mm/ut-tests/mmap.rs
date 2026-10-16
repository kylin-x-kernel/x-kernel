// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use linux_raw_sys::general::{MREMAP_DONTUNMAP, MREMAP_FIXED, MREMAP_MAYMOVE};
use unittest::def_test;

use super::MremapFlags;

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
