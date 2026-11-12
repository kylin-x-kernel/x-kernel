// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn dio_flags_start_empty() {
    assert!(DioMappingFlags::empty().is_empty());
    assert!(DioMappingFlags::NEW.contains(DioMappingFlags::NEW));
    assert_ne!(DioDirection::Read, DioDirection::Write);
}

#[def_test]
fn dio_mapping_flags_match_iomap_bits() {
    let all = DioMappingFlags::NEW
        | DioMappingFlags::DIRTY
        | DioMappingFlags::SHARED
        | DioMappingFlags::MERGED
        | DioMappingFlags::BUFFER_HEAD
        | DioMappingFlags::XATTR
        | DioMappingFlags::BOUNDARY
        | DioMappingFlags::ANON_WRITE
        | DioMappingFlags::ATOMIC_BIO
        | DioMappingFlags::INTEGRITY
        | DioMappingFlags::ZERO_TAIL
        | DioMappingFlags::FSVERITY
        | DioMappingFlags::PRIVATE;
    assert_eq!(all.bits().count_ones(), 13);
    assert_eq!(DioMappingFlags::PRIVATE.bits(), 1 << 12);
}

#[def_test]
fn dio_mapping_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DioMapping>();
}

#[def_test]
fn direct_io_operations_is_object_safe() {
    fn accepts_dynamic(_: &dyn DirectIoOperations) {}
    let _ = accepts_dynamic;
}