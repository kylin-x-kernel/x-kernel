// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::size_of;

use unittest::def_test;

use super::{FIEMAP_MAX_EXTENTS, FiemapExtent, FiemapHeader};

#[def_test]
fn fiemap_abi_layout_matches_linux_uapi() {
    assert_eq!(size_of::<FiemapHeader>(), 32);
    assert_eq!(size_of::<FiemapExtent>(), 56);
    assert_eq!(FIEMAP_MAX_EXTENTS, u32::MAX / 56);
}
