// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn ta_unix_socket_path_valid_uuid() {
    let path = ta_unix_socket_path("936da01f-9abd-4d9d-80c7-02af85c822a8").unwrap();
    assert_eq!(path, "/tmp/936da01f-9abd-4d9d-80c7-02af85c822a8.sock");
}

#[unittest::def_test]
fn ta_unix_socket_path_rejects_path_traversal() {
    assert_eq!(ta_unix_socket_path("../evil"), Err(TEE_ERROR_BAD_FORMAT));
}

#[unittest::def_test]
fn ta_unix_socket_path_rejects_slashes() {
    assert_eq!(
        ta_unix_socket_path("../../etc/passwd"),
        Err(TEE_ERROR_BAD_FORMAT)
    );
}
