// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! FAT metadata and name helpers.

use chrono::{DateTime, Datelike, Timelike};
use ktime_types::SystemTime;
use kvfs::{DeviceId, Metadata, MetadataUpdate, NodePermission, NodeType, VfsError};

use super::ff;

pub(crate) fn unix_to_dos(datetime: SystemTime) -> fatfs::DateTime {
    const DOS_MIN_SECONDS: i64 = 315_532_800;
    const DOS_MAX_SECONDS: i64 = 4_354_819_199;

    let unix_seconds = datetime
        .unix_seconds()
        .clamp(DOS_MIN_SECONDS, DOS_MAX_SECONDS);
    let nanoseconds = if unix_seconds == datetime.unix_seconds() {
        datetime.subsec_nanos()
    } else if unix_seconds == DOS_MIN_SECONDS {
        0
    } else {
        999_999_999
    };
    let dt = DateTime::from_timestamp(unix_seconds, nanoseconds)
        .expect("clamped FAT timestamp is representable by chrono");
    let dt = dt.naive_local();

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
    file.seek(fatfs::SeekFrom::Start(pos)).ok();
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

pub(crate) fn update_file_metadata(file: &mut ff::File, update: MetadataUpdate) {
    if let Some(atime) = update.atime {
        #[allow(deprecated)]
        file.set_accessed(unix_to_dos(atime).date);
    }
    if let Some(mtime) = update.mtime {
        #[allow(deprecated)]
        file.set_modified(unix_to_dos(mtime));
    }
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
