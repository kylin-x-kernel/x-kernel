// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Integration coverage for the nullable POSIX timer notification default.

use ksignal::Signo;
use ktimer::{PosixTimerCreateNotify, PosixTimerSigValue};
use posix_types::UserConstPtr;
use unittest::{assert, def_test};

use super::parse_sigevent;

#[def_test]
fn test_null_sigevent_preserves_default_signal() {
    assert!(matches!(
        parse_sigevent(UserConstPtr::default()),
        Ok(PosixTimerCreateNotify::Signal {
            signo: Signo::SIGALRM,
            target_tid: None,
            value: PosixTimerSigValue::TimerId,
        })
    ));
}
