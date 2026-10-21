// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Key derivation functions other than HKDF (PBKDF2, Concat KDF).

use alloc::{vec, vec::Vec};

use hmac::Hmac;
use pbkdf2::pbkdf2;
use sha1::Sha1;
use sm3::Sm3;

use crate::{
    error::{CryptoError, Result},
    hash::{Digest, HashAlgorithm, Sha1 as Sha1Digest, Sha224, Sha256, Sha384, Sha512},
};

type HmacSm3 = Hmac<Sm3>;
type Pbkdf2HmacSha1 = Hmac<Sha1>;

fn pbkdf2_derive<M>(
    password: &[u8],
    salt: &[u8],
    iterations: usize,
    dk_len: usize,
) -> Result<Vec<u8>>
where
    M: hmac::Mac + digest::KeyInit + digest::FixedOutput + Clone,
{
    if iterations == 0 || dk_len == 0 {
        return Err(CryptoError::InvalidInput);
    }
    let iterations: u32 = iterations
        .try_into()
        .map_err(|_| CryptoError::InvalidInput)?;
    let mut dk = vec![0u8; dk_len];
    pbkdf2::<M>(password, salt, iterations, &mut dk).map_err(|_| CryptoError::InvalidInput)?;
    Ok(dk)
}

/// PBKDF2-HMAC-SHA1 (OP-TEE `TEE_ALG_PBKDF2_HMAC_SHA1_DERIVE_KEY`).
pub fn pbkdf2_hmac_sha1(
    password: &[u8],
    salt: &[u8],
    iterations: usize,
    dk_len: usize,
) -> Result<Vec<u8>> {
    pbkdf2_derive::<Pbkdf2HmacSha1>(password, salt, iterations, dk_len)
}

/// PBKDF2-HMAC-SM3 (GmSSL encrypted PKCS#8 and related profiles).
pub fn pbkdf2_hmac_sm3(
    password: &[u8],
    salt: &[u8],
    iterations: usize,
    dk_len: usize,
) -> Result<Vec<u8>> {
    pbkdf2_derive::<HmacSm3>(password, salt, iterations, dk_len)
}

/// Concat KDF using the digest implementation selected by `HashAlgorithm`.
///
/// NIST SP 800-56A section 5.8.1; matches OP-TEE `tee_cryp_concat_kdf`.
pub fn concat_kdf(
    algorithm: HashAlgorithm,
    shared_secret: &[u8],
    other_info: &[u8],
    derived_key_len: usize,
) -> Result<Vec<u8>> {
    match algorithm {
        HashAlgorithm::Sha1 => {
            concat_kdf_digest::<Sha1Digest>(shared_secret, other_info, derived_key_len)
        }
        HashAlgorithm::Sha224 => {
            concat_kdf_digest::<Sha224>(shared_secret, other_info, derived_key_len)
        }
        HashAlgorithm::Sha256 => {
            concat_kdf_digest::<Sha256>(shared_secret, other_info, derived_key_len)
        }
        HashAlgorithm::Sha384 => {
            concat_kdf_digest::<Sha384>(shared_secret, other_info, derived_key_len)
        }
        HashAlgorithm::Sha512 => {
            concat_kdf_digest::<Sha512>(shared_secret, other_info, derived_key_len)
        }
        HashAlgorithm::Md5 | HashAlgorithm::Sm3 => Err(CryptoError::UnsupportedAlgorithm),
    }
}

/// Concat KDF with an explicit digest type.
pub fn concat_kdf_digest<D: Digest>(
    shared_secret: &[u8],
    other_info: &[u8],
    derived_key_len: usize,
) -> Result<Vec<u8>> {
    if derived_key_len == 0 {
        return Err(CryptoError::InvalidInput);
    }

    let hash_len = D::output_size();
    let n = derived_key_len / hash_len;
    let mut derived_key = Vec::with_capacity(derived_key_len);

    for i in 1..=(n + 1) {
        let mut chunk_len = hash_len;
        if i == n + 1 {
            chunk_len = derived_key_len % hash_len;
            if chunk_len == 0 {
                break;
            }
        }

        let counter = (i as u32).to_be_bytes();
        let mut hasher = D::new();
        hasher.update(&counter);
        hasher.update(shared_secret);
        if !other_info.is_empty() {
            hasher.update(other_info);
        }
        let digest = hasher.finalize();
        derived_key.extend_from_slice(&digest.as_bytes()[..chunk_len]);
    }

    debug_assert_eq!(derived_key.len(), derived_key_len);
    Ok(derived_key)
}
