// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT metadata and name helpers.

use chrono::{DateTime, Datelike, Timelike};
use ktime_types::SystemTime;
use kvfs::{DeviceId, Metadata, MetadataUpdate, NodePermission, NodeType, VfsError};

use super::ff;

pub(crate) const FAT_MIN_SECONDS: i64 = 315_532_800;
pub(crate) const FAT_MAX_SECONDS: i64 = 4_354_819_199;
pub(crate) const FAT_TIMESTAMP_LIMITS: ktime_types::TimestampLimits =
    ktime_types::TimestampLimits::new(1, FAT_MIN_SECONDS, FAT_MAX_SECONDS);

fn clamp_fat_timestamp(timestamp: SystemTime) -> SystemTime {
    FAT_TIMESTAMP_LIMITS.truncate(timestamp)
}

fn truncate_fat_atime(timestamp: SystemTime) -> SystemTime {
    const SECONDS_PER_DAY: i64 = 24 * 60 * 60;

    // X-Kernel currently has no FAT `time_offset` mount option or global
    // kernel timezone, so its local FAT time is UTC (offset zero).
    let seconds = clamp_fat_timestamp(timestamp).unix_seconds();
    SystemTime::from_unix_seconds(seconds - seconds.rem_euclid(SECONDS_PER_DAY))
}

fn truncate_fat_mtime(timestamp: SystemTime) -> SystemTime {
    let seconds = clamp_fat_timestamp(timestamp).unix_seconds();
    SystemTime::from_unix_seconds(seconds & !1)
}

fn unix_to_dos(datetime: SystemTime) -> fatfs::DateTime {
    let datetime = clamp_fat_timestamp(datetime);

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: clamp_fat_timestamp limits the value to \
                  the FAT range, which chrono represents"
    )]
    let dt = DateTime::from_timestamp(datetime.unix_seconds(), datetime.subsec_nanos())
        .expect("clamped FAT timestamp is representable by chrono");
    let dt = dt.naive_utc();

    fatfs::DateTime::new(
        fatfs::Date::new(dt.year() as _, dt.month() as _, dt.day() as _),
        fatfs::Time::new(
            dt.hour() as _,
            dt.minute() as _,
            dt.second() as _,
            dt.and_utc().timestamp_subsec_millis() as _,
        ),
    )
}

pub(crate) fn file_metadata(
    block_size: u64,
    inode: u64,
    file: &mut ff::File,
    node_type: NodeType,
) -> Metadata {
    use fatfs::Seek;
    let pos = file.seek(fatfs::SeekFrom::Current(0)).unwrap_or(0);
    let size = file.seek(fatfs::SeekFrom::End(0)).unwrap_or(0);
    // This best-effort size query keeps its result even if restoring the cursor fails.
    let _ = file.seek(fatfs::SeekFrom::Start(pos));
    Metadata {
        inode,
        device: 0,
        nlink: 1,
        mode: kvfs::Umode::new(node_type, NodePermission::default()),
        uid: 0,
        gid: 0,
        size,
        block_size: block_size as _,
        // TODO: The correct block count should be obtained from
        // `file.extents()`. However it would be costly. This implementation
        // would be enough for now.
        blocks: size / block_size,
        rdev: DeviceId::default(),
        atime: SystemTime::UNIX_EPOCH,
        mtime: SystemTime::UNIX_EPOCH,
        ctime: SystemTime::UNIX_EPOCH,
    }
}

pub(crate) fn normalize_file_metadata_update(mut update: MetadataUpdate) -> MetadataUpdate {
    update.mode = None;
    update.owner = None;
    update.atime = update.atime.map(truncate_fat_atime);
    if let Some(mtime) = update.mtime.map(truncate_fat_mtime) {
        update.mtime = Some(mtime);
        update.ctime = Some(mtime);
    } else {
        // FAT has no independent ctime field; an mtime update owns the shared
        // resident mtime/ctime value just as it owns the on-disk field.
        update.ctime = None;
    }
    update
}

pub(crate) fn update_file_metadata(file: &mut ff::File, update: MetadataUpdate) -> MetadataUpdate {
    let applied = normalize_file_metadata_update(update);
    if let Some(atime) = applied.atime {
        #[expect(
            deprecated,
            reason = "CG-024-lint-exceptions: deprecated: fatfs has no non-deprecated setter for \
                      explicit utimens access dates"
        )]
        file.set_accessed(unix_to_dos(atime).date);
    }
    if let Some(mtime) = applied.mtime {
        #[expect(
            deprecated,
            reason = "CG-024-lint-exceptions: deprecated: fatfs has no non-deprecated setter for \
                      explicit utimens modification times"
        )]
        file.set_modified(unix_to_dos(mtime));
    }
    applied
}

pub(crate) fn into_vfs_err<E>(err: fatfs::Error<E>) -> VfsError {
    use fatfs::Error::*;
    match err {
        AlreadyExists => VfsError::AlreadyExists,
        CorruptedFileSystem => VfsError::InvalidData,
        DirectoryIsNotEmpty => VfsError::DirectoryNotEmpty,
        InvalidFileNameLength => VfsError::NameTooLong,
        InvalidInput => VfsError::InvalidInput,
        UnsupportedFileNameCharacter => VfsError::InvalidData,
        NotEnoughSpace => VfsError::StorageFull,
        NotFound => VfsError::NotFound,
        UnexpectedEof | WriteZero => VfsError::Io,
        _ => VfsError::Io,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/util.rs");
