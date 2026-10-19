// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_path_normalization() {
    let path = PathBuf::from("/path/to/file");
    assert_eq!(path.as_str(), "/path/to/file");

    let root = PathBuf::from("/");
    assert_eq!(root.as_str(), "/");

    let empty = PathBuf::new();
    assert_eq!(empty.as_str(), "");
}

#[def_test]
fn test_path_normalization_complex() {
    let path = Pathname::new("/foo/bar/../baz/./qux/../file.txt");
    let normalized = path.normalize().unwrap();
    assert_eq!(normalized.as_str(), "/foo/baz/file.txt");

    let path = Pathname::new("/../../../test");
    assert!(path.normalize().is_none());

    let path = Pathname::new("././././test");
    let normalized = path.normalize().unwrap();
    assert_eq!(normalized.as_str(), "/test");
}
