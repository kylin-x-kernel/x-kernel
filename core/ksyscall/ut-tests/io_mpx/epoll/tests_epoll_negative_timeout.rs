// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use posix_types::{UserConstPtr, UserPtr};
use unittest::{assert_eq, def_test};

use super::sys_epoll_pwait;

#[def_test(user)]
fn test_negative_timeouts_reach_descriptor_validation() {
    for timeout in [i32::MIN, -65536, -2, -1] {
        assert_eq!(
            sys_epoll_pwait(
                -1,
                UserPtr::default(),
                1,
                timeout,
                UserConstPtr::default(),
                0,
            ),
            Err(KError::BadFileDescriptor)
        );
    }
}
