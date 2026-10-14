// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
fn exhausted_local_ids_return_errors_without_wrapping() {
    let mut ids = LocalIdAlloc { next: u16::MAX - 1 };
    assert_eq!(ids.alloc(), Ok(u16::MAX - 1));
    assert_eq!(ids.alloc(), Err(DriverError::NoMemory));
    assert_eq!(ids.alloc(), Err(DriverError::NoMemory));
    assert_eq!(ids.next, u16::MAX);
}
