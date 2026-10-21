// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stateless X25519 operations aligned with OP-TEE `crypto_acipher_x25519_*`.
//!
//! Multiplication uses the RFC 7748 clamped integer directly. Reducing that
//! integer modulo the prime-order subgroup order would clear the property that
//! the scalar is a multiple of 8, so low-order points would no longer map to
//! the all-zero u-coordinate.

use curve25519_dalek::{montgomery::MontgomeryPoint, scalar::clamp_integer};

use crate::{
    error::{CryptoError, Result},
    rng::CryptoRng,
};

/// X25519 key length in bytes (RFC 7748).
pub const X25519_KEY_SIZE_BYTES: usize = 32;

fn public_from_private(private_key: &[u8; X25519_KEY_SIZE_BYTES]) -> [u8; X25519_KEY_SIZE_BYTES] {
    MontgomeryPoint::mul_base_clamped(*private_key).to_bytes()
}

/// Generate an X25519 keypair (32-byte clamped private key + 32-byte public u-coordinate).
///
/// The returned private key is already clamped. [`x25519_shared_secret`] clamps
/// again, so either the raw seed or this stored form is a valid scalar input.
pub fn x25519_generate_keypair(rng: &mut dyn CryptoRng) -> Result<([u8; 32], [u8; 32])> {
    let mut private_key = [0u8; X25519_KEY_SIZE_BYTES];
    rng.fill_bytes(&mut private_key);
    private_key = clamp_integer(private_key);
    let public_key = public_from_private(&private_key);
    Ok((private_key, public_key))
}

/// Compute an X25519 shared secret (RFC 7748).
///
/// The private key is clamped and used as an integer. It is not reduced modulo
/// the group order. An all-zero result, including the image of a low-order
/// peer public key, returns [`CryptoError::VerificationFailed`].
pub fn x25519_shared_secret(
    private_key: &[u8; X25519_KEY_SIZE_BYTES],
    peer_public: &[u8; X25519_KEY_SIZE_BYTES],
) -> Result<[u8; X25519_KEY_SIZE_BYTES]> {
    let secret = MontgomeryPoint(*peer_public)
        .mul_clamped(*private_key)
        .to_bytes();
    if secret.iter().all(|byte| *byte == 0) {
        return Err(CryptoError::VerificationFailed);
    }
    Ok(secret)
}
