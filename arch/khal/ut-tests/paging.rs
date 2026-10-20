// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::MappingFlags;

#[def_test]
fn test_mapping_flags_combine() {
    let flags = MappingFlags::READ | MappingFlags::WRITE;
    assert!(flags.contains(MappingFlags::READ));
    assert!(flags.contains(MappingFlags::WRITE));
}

#[def_test]
fn test_mapping_flags_empty() {
    let flags = MappingFlags::empty();
    assert!(flags.is_empty());
}
