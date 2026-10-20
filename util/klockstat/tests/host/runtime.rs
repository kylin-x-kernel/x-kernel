// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::panic::Location;

use super::*;

#[test]
fn class_for_init_site_reuses_same_class() {
    fn lookup() -> &'static LockClassStats {
        class_for_init_site(Location::caller(), "Mutex")
    }

    let first = lookup();
    let second = lookup();
    assert!(core::ptr::eq(first, second));
    assert_eq!(first.location(), second.location());
}
