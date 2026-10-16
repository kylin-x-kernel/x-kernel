// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use posix_types::UserPtr;
use unittest::{assert_eq, def_test};

use super::*;

#[def_test(user)]
fn empty_path_cwd_matches_dot_metadata() {
    let empty = resolve_at(AT_FDCWD, Some(""), AT_EMPTY_PATH)
        .unwrap()
        .stat()
        .unwrap();
    let dot = resolve_at(AT_FDCWD, Some("."), 0).unwrap().stat().unwrap();
    assert_eq!((empty.dev, empty.ino), (dot.dev, dot.ino));
}

#[def_test(user)]
fn empty_path_requires_the_flag() {
    assert_eq!(
        resolve_at(AT_FDCWD, Some(""), 0).err(),
        Some(KError::NotFound)
    );
    // A nonempty relative path must still resolve its actual directory fd.
    assert_eq!(
        resolve_at(-123, Some("."), AT_EMPTY_PATH).err(),
        Some(KError::BadFileDescriptor)
    );
}

#[def_test(user)]
fn descriptor_only_operations_reject_cwd_sentinel() {
    assert_eq!(
        resolve_at(AT_FDCWD, None, AT_EMPTY_PATH).err(),
        Some(KError::BadFileDescriptor)
    );
    assert_eq!(
        crate::sys_fstat(AT_FDCWD, UserPtr::default()),
        Err(KError::BadFileDescriptor)
    );
    assert_eq!(
        crate::sys_fchmod(AT_FDCWD, 0o755),
        Err(KError::BadFileDescriptor)
    );
    assert_eq!(
        crate::sys_fchown(AT_FDCWD, -1, -1),
        Err(KError::BadFileDescriptor)
    );
    assert_eq!(
        crate::sys_flistxattr(AT_FDCWD, UserPtr::default(), 0),
        Err(KError::BadFileDescriptor)
    );
}
