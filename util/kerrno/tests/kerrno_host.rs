// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, KErrorKind, LinuxError};
use strum::EnumCount;

#[test]
fn test_try_from() {
    let max_code = KErrorKind::COUNT as i32;
    // `Stale` is appended last, so it owns the highest positional code; the
    // pre-existing variants keep their codes (new variants append only).
    assert_eq!(max_code, 47);
    assert_eq!(KError::FileTooLarge.code(), 45);
    assert_eq!(KError::IoWaitInterrupted.code(), 46);
    assert_eq!(max_code, KError::Stale.code());

    assert_eq!(KError::AddrInUse.code(), 1);
    assert_eq!(Ok(KError::AddrInUse), KError::try_from_i32(1));
    assert_eq!(Ok(KError::AlreadyConnected), KError::try_from_i32(2));
    assert_eq!(
        Ok(KError::ConnectionAborted),
        KError::try_from_i32(KError::ConnectionAborted.code())
    );
    assert_eq!(Ok(KError::Stale), KError::try_from_i32(max_code));
    assert_eq!(Err(max_code + 1), KError::try_from_i32(max_code + 1));
    assert_eq!(Err(0), KError::try_from_i32(0));
    assert_eq!(Err(i32::MAX), KError::try_from_i32(i32::MAX));
}

#[test]
fn test_stale_roundtrip() {
    // `Stale` maps to Linux ESTALE (116) in both directions. `KError` keeps
    // two encodings (positive kind code vs negative Linux errno), so the
    // roundtrip is asserted through the LinuxError projection, not by
    // comparing the two constructions.
    assert_eq!(KError::Stale.code(), 47);
    assert_eq!(LinuxError::from(KError::Stale), LinuxError::ESTALE);
    let err = KError::from(LinuxError::ESTALE);
    assert_eq!(LinuxError::from(err), LinuxError::ESTALE);
    assert_ne!(err.code(), KError::Stale.code());
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
