// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Integration coverage for zero-signal probes and checked signal rejection.

use kerrno::KError;
use linux_raw_sys::general::SI_TKILL;
use posix_types::UserConstPtr;
use unittest::{assert, def_test};

use super::{make_queue_signal_info, make_siginfo};

#[def_test]
fn test_zero_signal_preserves_probe_paths() {
    assert!(matches!(make_siginfo(0, SI_TKILL), Ok(None)));
    assert!(matches!(
        make_queue_signal_info(1, 0, UserConstPtr::default()),
        Ok(None)
    ));
}

#[def_test]
fn test_invalid_signal_rejected_before_current_or_usercopy() {
    for raw in [65_u32, 255, 256, 257, 320, 65_537, u32::MAX - 254, u32::MAX] {
        assert!(matches!(
            make_siginfo(raw, SI_TKILL),
            Err(KError::InvalidInput)
        ));
        assert!(matches!(
            make_queue_signal_info(1, raw, UserConstPtr::default()),
            Err(KError::InvalidInput)
        ));
    }
}
