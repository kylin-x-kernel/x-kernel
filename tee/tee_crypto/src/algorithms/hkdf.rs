// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! HKDF (HMAC-based Key Derivation Function) — RFC 5869.
//!
//! Implements HKDF-Extract and HKDF-Expand using any MAC that satisfies
//! the `Mac` trait (e.g., HMAC-SM3, HMAC-SHA256).

use alloc::vec::Vec;

use crate::{
    error::{CryptoError, Result},
    mac::Mac,
};

/// HKDF-Extract: PRK = HMAC-Hash(salt, IKM).
///
/// If `salt` is empty, a zero-filled string of hash output length is used.
pub fn hkdf_extract<M: Mac>(salt: &[u8], ikm: &[u8]) -> Result<Vec<u8>> {
    let actual_salt = if salt.is_empty() {
        alloc::vec![0u8; M::output_size()]
    } else {
        salt.to_vec()
    };
    let mut mac = M::new(&actual_salt)?;
    mac.update(ikm);
    Ok(mac.finalize())
}

/// HKDF-Expand: OKM = HKDF-Expand(PRK, info, L).
///
/// Produces `length` bytes of output keying material.
pub fn hkdf_expand<M: Mac>(prk: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>> {
    let hash_len = M::output_size();
    if length > 255 * hash_len {
        return Err(CryptoError::InvalidLength);
    }

    let n = length.div_ceil(hash_len);
    let mut okm = Vec::with_capacity(length);
    let mut t = Vec::new();

    for i in 1..=n {
        let mut mac = M::new(prk)?;
        mac.update(&t);
        mac.update(info);
        mac.update(&[i as u8]);
        t = mac.finalize();
        okm.extend_from_slice(&t);
    }

    okm.truncate(length);
    Ok(okm)
}

/// Convenience: full HKDF (extract + expand) in one call.
pub fn hkdf<M: Mac>(salt: &[u8], ikm: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>> {
    let prk = hkdf_extract::<M>(salt, ikm)?;
    hkdf_expand::<M>(&prk, info, length)
}
