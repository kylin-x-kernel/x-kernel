// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMCCC firmware TRNG entropy (ARM TRNG SMCCC 1.0).

use alloc::vec::Vec;

use kplat_aarch64::peripherals::trng::TrngReadError;

pub(crate) type ReadError = TrngReadError;

pub(crate) fn init() {
    kplat_aarch64::peripherals::trng::init_trng();
}

pub(crate) fn is_available() -> bool {
    kplat_aarch64::peripherals::trng::trng_available()
}

pub(crate) fn read(len: usize) -> Result<Vec<u8>, TrngReadError> {
    if len == 0 || !is_available() {
        return Err(TrngReadError::Unavailable);
    }

    let mut buf = alloc::vec![0u8; len];
    let read = kplat_aarch64::peripherals::trng::read_trng_random(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

unittest::path_mod_test!(tests, "../ut-tests/smccc_trng.rs");
