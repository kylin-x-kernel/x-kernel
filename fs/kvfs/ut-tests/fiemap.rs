// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::def_test;

use super::{FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags};
use crate::VfsResult;

#[derive(Default)]
struct CollectingWriter {
    extents: Vec<(u64, u64, u64, FiemapExtentFlags)>,
}

impl FiemapExtentWriter for CollectingWriter {
    fn write_extent(
        &mut self,
        index: u32,
        logical: u64,
        physical: u64,
        length: u64,
        flags: FiemapExtentFlags,
    ) -> VfsResult<()> {
        assert_eq!(usize::try_from(index).ok(), Some(self.extents.len()));
        self.extents.push((logical, physical, length, flags));
        Ok(())
    }
}

#[def_test]
fn fill_next_extent_adds_implied_flags() {
    let mut writer = CollectingWriter::default();
    let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 1, &mut writer);

    assert!(
        !info
            .fill_next_extent(0, 0, 4096, FiemapExtentFlags::DELALLOC)
            .expect("delayed extent should be valid")
    );
    assert_eq!(info.mapped_extents(), 1);
    assert!(
        writer.extents[0]
            .3
            .contains(FiemapExtentFlags::DELALLOC | FiemapExtentFlags::UNKNOWN)
    );
}

#[def_test]
fn zero_capacity_counts_until_last_extent() {
    let mut writer = CollectingWriter::default();
    let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 0, &mut writer);

    assert!(
        info.fill_next_extent(0, 4096, 4096, FiemapExtentFlags::empty())
            .expect("first extent should count")
    );
    assert!(
        !info
            .fill_next_extent(8192, 12288, 4096, FiemapExtentFlags::LAST)
            .expect("last extent should count")
    );
    assert_eq!(info.mapped_extents(), 2);
    assert!(writer.extents.is_empty());
}

#[def_test]
fn fill_next_extent_rejects_invalid_ranges() {
    let mut writer = CollectingWriter::default();
    let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), 1, &mut writer);

    assert!(
        info.fill_next_extent(0, 0, 0, FiemapExtentFlags::empty())
            .is_err()
    );
    assert!(
        info.fill_next_extent(u64::MAX, 0, 1, FiemapExtentFlags::empty())
            .is_err()
    );
    assert!(
        info.fill_next_extent(0, u64::MAX, 1, FiemapExtentFlags::empty())
            .is_err()
    );
}
