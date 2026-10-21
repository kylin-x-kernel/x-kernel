// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq};

use super::*;

fn file_exists(path: &str) -> bool {
    use crate::file::resolve_at;

    let loc = resolve_at(AT_FDCWD, Some(path), AT_EMPTY_PATH);
    matches!(loc, Ok(loc) if loc.stat().is_ok())
}

fn remove_dir(path: &str) -> TeeResult {
    FileVariant::remove_dir(path).inspect_err(|e| error!("remove dir failed: {:?}", e))
}

fn tee_get_file_size(path: &str) -> TeeResult<usize> {
    use crate::file::resolve_at;

    let loc = resolve_at(AT_FDCWD, Some(path), 0)
        .inspect_err(|e| error!("resolve_at failed: {:?}", e))
        .map_err(|_| TEE_ERROR_GENERIC)?;
    Ok(loc.stat().map_err(|_| TEE_ERROR_GENERIC)?.size as usize)
}

#[unittest::def_test(user)]
fn test_file_ops_read() {
    let fd = FileVariant::open("/tmp/test.txt", O_RDWR | O_CREAT, 0o644);
    assert!(fd.is_ok());
    let mut fd = fd.unwrap();
    let buf = [0xAA; 8];
    let n = fd.pwrite(&buf, 0).expect("Failed to pwrite file");
    assert_eq!(n, 8);
    let mut buf = [0; 8];
    let n = fd.pread(&mut buf, 0).expect("Failed to pread file");
    assert_eq!(n, 8);
    assert_eq!(buf, [0xAA; 8]);
    let mut buf = [0; 4];
    let n = fd.pread(&mut buf, 4).expect("Failed to pread file");
    assert_eq!(n, 4);
    assert_eq!(buf, [0xAA; 4]);
    let n = fd.pwrite(&[0xBB; 4], 4).expect("Failed to pwrite file");
    assert_eq!(n, 4);
    let mut buf = [0; 4];
    let n = fd.pread(&mut buf, 4).expect("Failed to pread file");
    assert_eq!(n, 4);
    assert_eq!(buf, [0xBB; 4]);
    fd.ftruncate(4).expect("Failed to truncate file");
    let size = tee_get_file_size("/tmp/test.txt").expect("Failed to get file size");
    assert_eq!(size, 4);
}

#[unittest::def_test(user)]
fn test_file_ops_exists() {
    let path = "/tmp/test.txt.not_exists";
    assert!(!file_exists(path));
    {
        let fd = FileVariant::open(path, O_RDWR | O_CREAT, 0o644);
        assert!(fd.is_ok());
    }
    assert!(file_exists(path));
    {
        let n = FileVariant::remove_file(path);
        assert!(n.is_ok());
    }
    assert!(!file_exists(path));
}

#[unittest::def_test(user)]
fn test_file_ops_create_dir() {
    let path = "/tmp/test_create_dir/";
    let n = FileVariant::create_dir(path);
    assert!(n.is_ok());
    assert!(file_exists(path));
    let n = FileVariant::create_dir(path);
    assert!(n.is_ok());
    let n = remove_dir(path);
    assert!(n.is_ok());
    assert!(!file_exists(path));
}
