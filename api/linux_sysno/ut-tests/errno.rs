// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;
use alloc::format;

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn basic() {
    assert_eq!(Errno::ENOENT.name(), Some("ENOENT"));
    assert_eq!(
        Errno::ENOENT.description(),
        Some("No such file or directory")
    );
    assert_eq!(
        format!("{}", Errno::ENOENT),
        "-2 ENOENT (No such file or directory)"
    );
    assert_eq!(format!("{:?}", Errno::ENOENT), "ENOENT");
}

#[def_test]
fn from_ret() {
    assert_eq!(Errno::from_ret(-2isize as usize), Err(Errno::ENOENT));
    assert_eq!(Errno::from_ret(2), Ok(2));
}
