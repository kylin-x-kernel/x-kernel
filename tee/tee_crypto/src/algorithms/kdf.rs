// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Password-based key derivation (PBKDF2).

use alloc::{vec, vec::Vec};

use hmac::Hmac;
use pbkdf2::pbkdf2;
use sm3::Sm3;

use crate::error::{CryptoError, Result};

type HmacSm3 = Hmac<Sm3>;

/// PBKDF2-HMAC-SM3 (GmSSL encrypted PKCS#8 and related profiles).
pub fn pbkdf2_hmac_sm3(
    password: &[u8],
    salt: &[u8],
    iterations: usize,
    dk_len: usize,
) -> Result<Vec<u8>> {
    if iterations == 0 || dk_len == 0 {
        return Err(CryptoError::InvalidInput);
    }
    let iterations: u32 = iterations
        .try_into()
        .map_err(|_| CryptoError::InvalidInput)?;
    let mut dk = vec![0u8; dk_len];
    pbkdf2::<HmacSm3>(password, salt, iterations, &mut dk)
        .map_err(|_| CryptoError::InvalidInput)?;
    Ok(dk)
}
