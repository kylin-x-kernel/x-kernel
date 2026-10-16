// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn xattr_list_is_nul_separated_and_bounded() {
    let mut writer = XattrListWriter::new(64);
    writer
        .emit(XattrNameRef::new(b"user.alpha").unwrap())
        .unwrap();
    writer
        .emit(XattrNameRef::new(b"security.beta").unwrap())
        .unwrap();
    assert_eq!(writer.bytes, b"user.alpha\0security.beta\0");
}

#[def_test]
fn xattr_list_size_query_does_not_materialize_names() {
    let mut writer = XattrListWriter::new(0);
    writer
        .emit(XattrNameRef::new(b"user.alpha").unwrap())
        .unwrap();

    assert_eq!(writer.required_len, b"user.alpha\0".len());
    assert!(writer.bytes.is_empty());
}
