// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_iovec_layout() {
    assert_eq!(
        core::mem::size_of::<IoVec>(),
        core::mem::size_of::<*mut u8>() + core::mem::size_of::<isize>()
    );
}
