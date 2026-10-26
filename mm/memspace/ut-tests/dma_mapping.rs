// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn dma_modes_preserve_existing_permissions_without_execute_or_user() {
    let normal = MappingFlags::READ | MappingFlags::WRITE;
    assert_eq!(mapping_flags(DmaMappingMode::Normal), normal);
    assert_eq!(
        mapping_flags(DmaMappingMode::Coherent),
        normal | MappingFlags::UNCACHED | MappingFlags::SHARED
    );
}
