// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn metadata_update_returns_fat_field_granularity() {
    const DAY: i64 = 24 * 60 * 60;
    let day = FAT_MIN_SECONDS + 10 * DAY;
    let applied = normalize_file_metadata_update(MetadataUpdate {
        atime: Some(SystemTime::from_unix_parts(day + 13 * 60 * 60 + 15, 123).unwrap()),
        mtime: Some(SystemTime::from_unix_parts(day + 17, 456).unwrap()),
        ctime: Some(SystemTime::from_unix_parts(day + 99, 789).unwrap()),
        ..Default::default()
    });

    assert_eq!(applied.atime, Some(SystemTime::from_unix_seconds(day)));
    assert_eq!(applied.mtime, Some(SystemTime::from_unix_seconds(day + 16)));
    assert_eq!(applied.ctime, applied.mtime);
}

#[def_test]
fn metadata_update_clamps_fat_range_and_ignores_independent_ctime() {
    let below_range = SystemTime::from_unix_seconds(FAT_MIN_SECONDS - 1);
    let above_range = SystemTime::from_unix_seconds(FAT_MAX_SECONDS + 1);
    let applied = normalize_file_metadata_update(MetadataUpdate {
        atime: Some(below_range),
        mtime: Some(above_range),
        ..Default::default()
    });

    assert_eq!(
        applied.atime,
        Some(SystemTime::from_unix_seconds(FAT_MIN_SECONDS))
    );
    assert_eq!(
        applied.mtime,
        Some(SystemTime::from_unix_seconds(FAT_MAX_SECONDS & !1))
    );
    assert_eq!(applied.ctime, applied.mtime);

    let ctime_only = normalize_file_metadata_update(MetadataUpdate {
        ctime: Some(SystemTime::from_unix_seconds(FAT_MIN_SECONDS)),
        ..Default::default()
    });
    assert_eq!(ctime_only.ctime, None);
}
