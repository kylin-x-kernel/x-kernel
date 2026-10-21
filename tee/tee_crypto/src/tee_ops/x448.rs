// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stateless X448 operations aligned with OP-TEE `crypto_acipher_x448_*`.

use x448::{PublicKey, StaticSecret};

use crate::{
    error::{CryptoError, Result},
    rng::CryptoRng,
};

/// X448 key length in bytes (RFC 7748).
pub const X448_KEY_SIZE_BYTES: usize = 56;

/// Generate an X448 keypair (56-byte private seed + 56-byte public u-coordinate).
pub fn x448_generate_keypair(rng: &mut dyn CryptoRng) -> Result<([u8; 56], [u8; 56])> {
    let secret = StaticSecret::random_from_rng(rng);
    let public = PublicKey::from(&secret);
    Ok((*secret.as_bytes(), *public.as_bytes()))
}

/// Compute an X448 shared secret (RFC 7748).
pub fn x448_shared_secret(
    private_key: &[u8; X448_KEY_SIZE_BYTES],
    peer_public: &[u8; X448_KEY_SIZE_BYTES],
) -> Result<[u8; X448_KEY_SIZE_BYTES]> {
    let secret = StaticSecret::from(*private_key);
    let peer = PublicKey::from_bytes(peer_public).ok_or(CryptoError::InvalidKey)?;
    let shared = secret.diffie_hellman(&peer);
    let secret_bytes = *shared.as_bytes();
    if secret_bytes.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::VerificationFailed);
    }
    Ok(secret_bytes)
}
