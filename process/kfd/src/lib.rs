// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process file-descriptor runtime.

#![no_std]

extern crate alloc;

mod fd_table;
mod file_descriptor;
mod file_like;
mod stat;

pub use self::{
    fd_table::FdTable,
    file_descriptor::{FdSnapshot, FileDescriptor},
    file_like::{FileLike, IoDst, IoSrc, ReadBuf, WriteBuf},
    stat::Kstat,
};

#[cfg(unittest)]
mod tests {
    use alloc::{borrow::Cow, sync::Arc};
    use core::{task::Context, time::Duration};

    use kpoll::{IoEvents, Pollable};
    use kvfs::DeviceId;
    use linux_raw_sys::general::stat;
    use unittest::def_test;

    use crate::{FdTable, FileLike, Kstat};

    struct SnapshotTestFile;

    impl Pollable for SnapshotTestFile {
        fn poll(&self) -> IoEvents {
            IoEvents::IN
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileLike for SnapshotTestFile {
        fn path(&self) -> Cow<'_, str> {
            Cow::Borrowed("/snapshot-test")
        }

        fn open_flags(&self) -> u32 {
            0x2000
        }
    }

    #[def_test]
    fn test_kstat_default() {
        let kstat = Kstat::default();
        assert_eq!(kstat.dev, 0);
        assert_eq!(kstat.ino, 1);
        assert_eq!(kstat.nlink, 1);
        assert_eq!(kstat.mode, 0);
        assert_eq!(kstat.uid, 1);
        assert_eq!(kstat.gid, 1);
        assert_eq!(kstat.size, 0);
        assert_eq!(kstat.blksize, 4096);
        assert_eq!(kstat.blocks, 0);
        assert_eq!(kstat.atime, Duration::default());
        assert_eq!(kstat.mtime, Duration::default());
        assert_eq!(kstat.ctime, Duration::default());
    }

    #[def_test]
    fn test_kstat_to_stat() {
        let kstat = Kstat {
            dev: 42,
            ino: 100,
            nlink: 3,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            size: 4096,
            blksize: 512,
            blocks: 8,
            rdev: DeviceId::default(),
            atime: Duration::new(1000, 500_000_000),
            mtime: Duration::new(2000, 0),
            ctime: Duration::new(3000, 123_456_789),
        };

        let s: stat = kstat.into();
        assert_eq!(s.st_dev, 42);
        assert_eq!(s.st_ino, 100);
        assert_eq!(s.st_nlink, 3);
        assert_eq!(s.st_mode, 0o755);
        assert_eq!(s.st_uid, 1000);
        assert_eq!(s.st_gid, 1000);
        assert_eq!(s.st_size, 4096);
        assert_eq!(s.st_blksize, 512);
        assert_eq!(s.st_blocks, 8);
        assert_eq!(s.st_atime, 1000);
        assert_eq!(s.st_atime_nsec, 500_000_000);
        assert_eq!(s.st_mtime, 2000);
        assert_eq!(s.st_mtime_nsec, 0);
        assert_eq!(s.st_ctime, 3000);
        assert_eq!(s.st_ctime_nsec, 123_456_789);
    }

    #[def_test]
    fn test_fd_snapshot_keeps_file_like_alive_after_close() {
        let mut table = FdTable::default();
        let fd = table
            .add_file_like(16, Arc::new(SnapshotTestFile), true)
            .unwrap();

        let snapshot = table.snapshot(fd).unwrap();
        table.close_file_like(fd).unwrap();

        assert_eq!(snapshot.fd(), fd);
        assert!(snapshot.cloexec());
        assert_eq!(snapshot.open_flags(), 0x2000);
        assert_eq!(snapshot.path(), "/snapshot-test");
        assert!(snapshot.inner().poll().contains(IoEvents::IN));
        assert!(matches!(
            table.snapshot(fd),
            Err(kerrno::KError::BadFileDescriptor)
        ));
    }
}
