// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, KErrorKind, LinuxError};
use strum::EnumCount;

#[test]
fn test_try_from() {
    let max_code = KErrorKind::COUNT as i32;
    assert_eq!(max_code, 46);
    assert_eq!(KError::FileTooLarge.code(), 45);
    assert_eq!(max_code, KError::IoWaitInterrupted.code());

    assert_eq!(KError::AddrInUse.code(), 1);
    assert_eq!(Ok(KError::AddrInUse), KError::try_from_i32(1));
    assert_eq!(Ok(KError::AlreadyConnected), KError::try_from_i32(2));
    assert_eq!(
        Ok(KError::ConnectionAborted),
        KError::try_from_i32(KError::ConnectionAborted.code())
    );
    assert_eq!(
        Ok(KError::IoWaitInterrupted),
        KError::try_from_i32(max_code)
    );
    assert_eq!(Err(max_code + 1), KError::try_from_i32(max_code + 1));
    assert_eq!(Err(0), KError::try_from_i32(0));
    assert_eq!(Err(i32::MAX), KError::try_from_i32(i32::MAX));
}

#[test]
fn test_conversion() {
    for i in 1.. {
        let err = LinuxError::new(i);
        if err.name().is_none() {
            break;
        }
        assert_eq!(err.into_raw(), i);
        let e = KError::from(err);
        assert_eq!(e.code(), -i);
        assert_eq!(LinuxError::from(e), err);
    }
}
