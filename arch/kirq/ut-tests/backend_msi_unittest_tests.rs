// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{alloc_msix, free_msix};
use crate::IrqAffinity;

#[def_test]
fn test_msix_is_unsupported_without_backend() {
    assert!(alloc_msix(IrqAffinity::Any).is_none());
    assert!(!free_msix(0xffff_ffff));
}
