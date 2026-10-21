// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn validate_tee_path_allows_tee_root() {
    assert_eq!(
        validate_tee_path("/tee/object.bin").unwrap(),
        "/tee/object.bin"
    );
}

#[unittest::def_test]
fn validate_tee_path_allows_tee_directory_root() {
    // Trailing slash is stripped by normalize(); the root itself must still match.
    assert_eq!(validate_tee_path("/tee/").unwrap(), "/tee");
}

#[unittest::def_test]
fn validate_tee_path_allows_tmp_root() {
    assert_eq!(validate_tee_path("/tmp/test.txt").unwrap(), "/tmp/test.txt");
}

#[unittest::def_test]
fn validate_tee_path_allows_tmp_directory_root() {
    assert_eq!(validate_tee_path("/tmp/").unwrap(), "/tmp");
}

#[unittest::def_test]
fn validate_tee_path_rejects_parent_dir() {
    assert!(validate_tee_path("/tee/../etc/passwd").is_err());
    assert!(validate_tee_path("../etc/passwd").is_err());
}

#[unittest::def_test]
fn validate_tee_path_rejects_outside_roots() {
    assert!(validate_tee_path("/etc/passwd").is_err());
    assert!(validate_tee_path("/var/log/messages").is_err());
}

#[unittest::def_test]
fn validate_tee_path_rejects_relative_paths() {
    assert!(validate_tee_path("test.txt").is_err());
}
