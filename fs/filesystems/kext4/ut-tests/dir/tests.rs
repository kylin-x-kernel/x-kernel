// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn unknown_dirent_file_type_scans_as_unknown() {
    let mut bytes = [0; 12];
    put_u32(&mut bytes, 0x00, 2);
    put_u16(&mut bytes, 0x04, 12);
    bytes[0x06] = 1;
    bytes[0x07] = 99;
    bytes[0x08] = b"x"[0];

    let mut seen = None;
    let completed =
        scan_directory_block(&bytes, 4_096, 0, Ext4DirPos::new(0), 10, &mut |entry| {
            seen = Some((Vec::from(entry.name), entry.file_type));
            Ok(true)
        })
        .expect("scan directory block");

    assert!(completed);
    assert_eq!(
        seen,
        Some((Vec::from(&b"x"[..]), DirectoryFileType::Unknown))
    );
}

#[def_test]
fn non_boundary_start_position_is_invalid() {
    let mut bytes = [0; 12];
    put_u32(&mut bytes, 0x00, 2);
    put_u16(&mut bytes, 0x04, 12);
    bytes[0x06] = 1;
    bytes[0x07] = 1;
    bytes[0x08] = b'x';

    let mut emitted = false;
    let error = scan_directory_block(&bytes, 4_096, 0, Ext4DirPos::new(1), 10, &mut |_| {
        emitted = true;
        Ok(true)
    })
    .expect_err("reject non-boundary directory position");

    assert_eq!(error, Ext4Error::InvalidDirectoryPosition);
    assert!(!emitted);
}

#[def_test]
fn directory_stream_resumes_without_losing_entries_and_rejects_non_boundary() {
    use crate::superblock::test_fixtures::recovery_test_filesystem;
    struct OneEntry(Option<Vec<u8>>);
    impl Ext4DirSink for OneEntry {
        fn emit(&mut self, entry: Ext4DirEntryRef<'_>, _: Ext4DirPos) -> Ext4Result<bool> {
            if self.0.is_some() {
                return Ok(false);
            }
            self.0 = Some(Vec::from(entry.name_bytes()));
            Ok(true)
        }
    }
    let (mut filesystem, _) = recovery_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .unwrap();
    for name in [b"alpha".as_slice(), b"beta", b"gamma"] {
        filesystem
            .create_regular_file(&root, name, 0o644, 0, 0, crate::Ext4Timestamp::new(1, 0))
            .unwrap();
    }
    let expected: Vec<_> = filesystem
        .read_dir(&root)
        .unwrap()
        .into_iter()
        .map(|entry| Vec::from(entry.name_bytes()))
        .collect();
    let mut position = Ext4DirPos::new(0);
    let mut resumed = Vec::new();
    // One final call verifies EOF. A bound prevents a broken iterator
    // from hanging the kernel test runner.
    for _ in 0..=expected.len() {
        let mut sink = OneEntry(None);
        let next = filesystem
            .read_dir_from(&root, position, &mut sink)
            .unwrap();
        match sink.0 {
            Some(name) => {
                assert!(next.get() > position.get());
                resumed.push(name);
            }
            None => {
                assert_eq!(next, position);
                break;
            }
        }
        position = next;
    }
    assert_eq!(resumed, expected);
    let mut sink = OneEntry(None);
    assert_eq!(
        filesystem.read_dir_from(&root, Ext4DirPos::new(1), &mut sink),
        Err(Ext4Error::InvalidDirectoryPosition)
    );
    assert!(sink.0.is_none());
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
