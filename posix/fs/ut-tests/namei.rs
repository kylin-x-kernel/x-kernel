// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn readlinkat_rejects_nonpositive_length_before_user_access() {
    for size in [0, -1, i32::MIN] {
        assert_eq!(
            sys_readlinkat(AT_FDCWD, UserConstPtr::default(), UserPtr::default(), size),
            Err(KError::InvalidInput)
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[def_test]
fn readlink_rejects_nonpositive_length_before_user_access() {
    for size in [0, -1, i32::MIN] {
        assert_eq!(
            sys_readlink(UserConstPtr::default(), UserPtr::default(), size),
            Err(KError::InvalidInput)
        );
    }
}
