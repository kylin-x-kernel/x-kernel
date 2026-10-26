// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};

use unittest::{assert_eq, def_test};

use super::*;
use crate::{DirMapping, FileSystemType, Filename, Mount, NodePermission, SimpleDir, SimpleFs};

static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::internal("simple-file-test");

#[def_test]
fn command_file_read_rejects_missing_content() {
    let missing = CommandFile::new(|_| Ok(None::<Vec<u8>>));
    assert_eq!(missing.read_all(), Err(VfsError::InvalidData));
    let failed = CommandFile::new(|_| Err::<Option<Vec<u8>>, _>(VfsError::PermissionDenied));
    assert_eq!(failed.read_all(), Err(VfsError::PermissionDenied));
    let empty = CommandFile::new(|_| Ok(Some(Vec::<u8>::new())));
    assert_eq!(empty.read_all(), Ok(Cow::Owned(Vec::new())));
}

#[def_test]
fn snapshot_partial_reads_are_stable_across_source_changes() {
    let source = Arc::new(crate::Mutex::new(b"abcdefgh".to_vec()));
    let observed = source.clone();
    let generations = Arc::new(crate::Mutex::new(0usize));
    let generated = generations.clone();
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add(
            "snapshot",
            SimpleFile::new_snapshot(fs.clone(), move || {
                *generated.lock() += 1;
                Ok(observed.lock().clone())
            }),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();
    let open = |flags| {
        Filename::new("/snapshot")
            .open_with_flags_at(
                &root,
                &root,
                flags,
                NodePermission::empty(),
                NodePermission::empty(),
                kcred::initial_cred(),
            )
            .unwrap()
    };
    let first = open(linux_raw_sys::general::O_RDONLY);
    let mut prefix = [0; 3];
    assert_eq!(first.read(&mut prefix), Ok(3));
    assert_eq!(&prefix, b"abc");
    *source.lock() = b"CHANGED-LONG".to_vec();
    let mut tail = [0; 5];
    assert_eq!(first.read(&mut tail), Ok(5));
    assert_eq!(&tail, b"defgh");
    let mut next = [0; 12];
    assert_eq!(
        open(linux_raw_sys::general::O_RDONLY).read(&mut next),
        Ok(12)
    );
    assert_eq!(&next, b"CHANGED-LONG");
    assert_eq!(first.llseek(0, crate::file::SEEK_END), Ok(8));
    assert_eq!(first.read(&mut next), Ok(0));
    assert_eq!(first.llseek(-2, crate::file::SEEK_END), Ok(6));
    assert_eq!(first.read(&mut next), Ok(2));
    assert_eq!(&next[..2], b"gh");
    assert_eq!(
        first.llseek(-9, crate::file::SEEK_END),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(first.llseek(0, crate::file::SEEK_CUR), Ok(8));
    assert_eq!(
        first.llseek(i64::MAX, crate::file::SEEK_END),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(first.llseek(0, -1), Err(VfsError::InvalidInput));
    assert_eq!(first.llseek(0, crate::file::SEEK_CUR), Ok(8));
    assert_eq!(first.llseek(0, crate::file::SEEK_SET), Ok(0));
    assert_eq!(
        open(linux_raw_sys::general::O_RDONLY).llseek(0, crate::file::SEEK_END),
        Ok(12)
    );
    assert_eq!(first.write(b"x"), Err(VfsError::BadFileDescriptor));
    assert_eq!(*generations.lock(), 3);
    let write_opened = open(linux_raw_sys::general::O_WRONLY);
    assert_eq!(*generations.lock(), 3);
    assert_eq!(
        write_opened
            .private_data_get::<SimpleFileSnapshot>()
            .is_none(),
        true
    );
    assert_eq!(write_opened.llseek(3, crate::file::SEEK_SET), Ok(3));
    assert_eq!(write_opened.llseek(2, crate::file::SEEK_CUR), Ok(5));
    assert_eq!(write_opened.llseek(-1, crate::file::SEEK_CUR), Ok(4));
    assert_eq!(
        write_opened.llseek(-5, crate::file::SEEK_CUR),
        Err(VfsError::InvalidInput)
    );
    assert_eq!(
        write_opened.llseek(0, crate::file::SEEK_END),
        Err(VfsError::InvalidData)
    );
    assert_eq!(write_opened.llseek(0, crate::file::SEEK_CUR), Ok(4));
    assert_eq!(*generations.lock(), 3);
    assert_eq!(write_opened.write(b"x"), Err(VfsError::InvalidInput));
    assert_eq!(*source.lock(), b"CHANGED-LONG".to_vec());
    let read_write = open(linux_raw_sys::general::O_RDWR);
    assert_eq!(*generations.lock(), 4);
    assert_eq!(read_write.read(&mut next), Ok(12));
    assert_eq!(&next, b"CHANGED-LONG");
    assert_eq!(read_write.write(b"x"), Err(VfsError::InvalidInput));
    source.lock().clear();
    let empty = open(linux_raw_sys::general::O_RDONLY);
    assert_eq!(empty.llseek(0, crate::file::SEEK_END), Ok(0));
    assert_eq!(empty.read(&mut next), Ok(0));
    assert_eq!(empty.llseek(2, crate::file::SEEK_SET), Ok(2));
    assert_eq!(empty.read(&mut next), Ok(0));
    assert_eq!(first.llseek(0, crate::file::SEEK_END), Ok(8));
}

#[def_test]
fn command_file_consumes_each_write_independently_of_offset() {
    let commands = Arc::new(crate::Mutex::new(Vec::<Vec<u8>>::new()));
    let observed = commands.clone();
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, move |fs| {
        let mut root = DirMapping::new();
        root.add(
            "command",
            SimpleFile::new_regular_with_permission(
                fs.clone(),
                NodePermission::from_bits_truncate(0o222),
                CommandFile::new(move |operation| match operation {
                    SimpleFileOperation::Read => Ok(Some(Vec::new())),
                    SimpleFileOperation::Write { data, .. } => {
                        observed.lock().push(data.to_vec());
                        Ok(None)
                    }
                }),
            ),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/command")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_WRONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    assert_eq!(file.write(b"first"), Ok(5));
    assert_eq!(file.position(), 5);
    assert_eq!(file.inode().size(), 0);
    assert_eq!(file.write(b""), Ok(0));
    assert_eq!(file.write(b"second"), Ok(6));
    let mut positioned_offset = 123;
    assert_eq!(
        file.write_from(b"positioned", &mut positioned_offset),
        Ok(10)
    );
    assert_eq!(positioned_offset, 133);
    assert_eq!(file.inode().size(), 0);
    assert_eq!(
        *commands.lock(),
        vec![
            b"first".to_vec(),
            b"second".to_vec(),
            b"positioned".to_vec()
        ]
    );
}

/// `/proc` and `/sys` (and cgroup v2) serve their attribute files as
/// `SimpleFile`s, and all three Linux file operation sets provide a poll
/// operation even when the entry has no handler of its own
/// (`proc_reg_file_ops`/`proc_reg_poll`, `kernfs_file_fops`/`kernfs_fop_poll`,
/// both falling back to `DEFAULT_POLLMASK`). `epoll_ctl` asks only whether such
/// a handler exists, so a generated file must answer yes.
#[def_test]
fn generated_file_provides_the_linux_poll_fallback() {
    let super_block = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        let mut root = DirMapping::new();
        root.add(
            "status",
            SimpleFile::new_regular(fs.clone(), || Ok(String::from("ok\n"))),
        );
        SimpleDir::new_maker(fs, Arc::new(root))
    });
    let root = Mount::new_root(&super_block).root_path();
    let file = Filename::new("/status")
        .open_with_flags_at(
            &root,
            &root,
            linux_raw_sys::general::O_RDONLY,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();

    assert!(file.can_poll());
    let expected = IoEvents::IN | IoEvents::OUT;
    assert_eq!(
        file.poll_handler().map(|events| events.bits()),
        Some(expected.bits())
    );
}
