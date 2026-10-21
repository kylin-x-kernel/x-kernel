// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::KError;
use unittest::{assert_eq, def_test};

use super::sys_pidfd_getfd;

#[def_test]
fn pidfd_getfd_rejects_reserved_flags_before_descriptor_lookup() {
    for bit in 0..u32::BITS {
        assert_eq!(
            sys_pidfd_getfd(-1, -1, 1u32 << bit),
            Err(KError::InvalidInput)
        );
    }
    assert_eq!(sys_pidfd_getfd(-1, -1, u32::MAX), Err(KError::InvalidInput));
}
