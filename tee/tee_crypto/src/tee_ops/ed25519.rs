// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stateless Ed25519 operations aligned with OP-TEE `crypto_acipher_ed25519_*`.

use alloc::vec::Vec;

use curve25519_dalek::{
    edwards::{CompressedEdwardsY, EdwardsPoint},
    scalar::Scalar,
};
use ed25519_dalek::{Digest, Sha512, Signer, SigningKey, VerifyingKey, hazmat::ExpandedSecretKey};

use crate::{
    error::{CryptoError, Result},
    material::{SignatureAlgorithm, SignatureBytes, SignatureEncoding},
    rng::CryptoRng,
};

/// Ed25519 private key seed length in bytes.
pub const ED25519_KEY_SIZE_BYTES: usize = 32;

/// Ed25519 signature length in bytes.
pub const ED25519_SIGNATURE_SIZE_BYTES: usize = 64;

/// Maximum Ed25519 context string length accepted by the TEE API.
pub const ED25519_CTX_MAX_LENGTH: usize = 255;

const DOM2_PREFIX: &[u8] = b"SigEd25519 no Ed25519 collisions";

/// Ed25519 sign/verify variant selected through TEE operation parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ed25519Variant {
    pub prehash: bool,
    pub context: Option<Vec<u8>>,
}

impl Ed25519Variant {
    pub fn pure() -> Self {
        Self {
            prehash: false,
            context: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(ctx) = &self.context
            && ctx.len() > ED25519_CTX_MAX_LENGTH
        {
            return Err(CryptoError::InvalidLength);
        }
        Ok(())
    }
}

fn verifying_key_from_bytes(public_key: &[u8; ED25519_KEY_SIZE_BYTES]) -> Result<VerifyingKey> {
    VerifyingKey::from_bytes(public_key).map_err(|_| CryptoError::InvalidKey)
}

fn signature_from_bytes(signature: &[u8]) -> Result<ed25519_dalek::Signature> {
    if signature.len() != ED25519_SIGNATURE_SIZE_BYTES {
        return Err(CryptoError::InvalidLength);
    }
    Ok(ed25519_dalek::Signature::from_bytes(
        signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?,
    ))
}

fn expanded_secret_key(seed: &[u8; ED25519_KEY_SIZE_BYTES]) -> ExpandedSecretKey {
    let hash = Sha512::default().chain_update(seed).finalize();
    let bytes: [u8; 64] = hash.into();
    ExpandedSecretKey::from_bytes(&bytes)
}

fn update_dom2<D: Digest>(hash: &mut D, ph_flag: u8, ctx: &[u8]) -> Result<()> {
    if ctx.len() > ED25519_CTX_MAX_LENGTH {
        return Err(CryptoError::InvalidLength);
    }
    hash.update(DOM2_PREFIX);
    hash.update([ph_flag]);
    hash.update([ctx.len() as u8]);
    hash.update(ctx);
    Ok(())
}

/// RFC 8032 Ed25519ctx (`F = 0`): sign with domain-separated context, message not prehashed.
fn sign_ed25519ctx(
    signing_key: &SigningKey,
    message: &[u8],
    ctx: &[u8],
) -> Result<ed25519_dalek::Signature> {
    let esk = expanded_secret_key(&signing_key.to_bytes());
    let verifying_key = signing_key.verifying_key();

    let mut hash = Sha512::new();
    update_dom2(&mut hash, 0, ctx)?;
    hash.update(esk.hash_prefix);
    hash.update(message);
    let r = Scalar::from_hash(hash);
    let r_compressed = EdwardsPoint::mul_base(&r).compress();

    let mut hash = Sha512::new();
    update_dom2(&mut hash, 0, ctx)?;
    hash.update(r_compressed.as_bytes());
    hash.update(verifying_key.as_bytes());
    hash.update(message);
    let k = Scalar::from_hash(hash);
    let s = k * esk.scalar + r;

    let mut sig_bytes = [0u8; ED25519_SIGNATURE_SIZE_BYTES];
    sig_bytes[..32].copy_from_slice(r_compressed.as_bytes());
    sig_bytes[32..].copy_from_slice(s.as_bytes());
    Ok(ed25519_dalek::Signature::from_bytes(&sig_bytes))
}

/// RFC 8032 Ed25519ctx verify with strict scalar / small-order checks.
fn verify_ed25519ctx(
    verifying_key: &VerifyingKey,
    message: &[u8],
    signature: &ed25519_dalek::Signature,
    ctx: &[u8],
) -> Result<()> {
    let sig_bytes = signature.to_bytes();
    let r_compressed =
        CompressedEdwardsY::from_slice(&sig_bytes[..32]).map_err(|_| CryptoError::InvalidInput)?;
    let s = Option::from(Scalar::from_canonical_bytes(
        sig_bytes[32..]
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?,
    ))
    .ok_or(CryptoError::VerificationFailed)?;

    let signature_r = r_compressed
        .decompress()
        .ok_or(CryptoError::VerificationFailed)?;
    if signature_r.is_small_order() || verifying_key.to_edwards().is_small_order() {
        return Err(CryptoError::VerificationFailed);
    }

    let mut hash = Sha512::new();
    update_dom2(&mut hash, 0, ctx)?;
    hash.update(r_compressed.as_bytes());
    hash.update(verifying_key.as_bytes());
    hash.update(message);
    let k = Scalar::from_hash(hash);

    let expected_r =
        EdwardsPoint::vartime_double_scalar_mul_basepoint(&k, &-verifying_key.to_edwards(), &s)
            .compress();

    if expected_r == r_compressed {
        Ok(())
    } else {
        Err(CryptoError::VerificationFailed)
    }
}

fn sign_with_variant(
    signing_key: &SigningKey,
    message: &[u8],
    variant: &Ed25519Variant,
) -> Result<SignatureBytes> {
    variant.validate()?;

    let signature = if variant.prehash {
        let mut hasher = Sha512::new();
        hasher.update(message);
        signing_key
            .sign_prehashed(hasher, variant.context.as_deref())
            .map_err(|_| CryptoError::InternalError)?
    } else if let Some(ctx) = variant.context.as_deref() {
        sign_ed25519ctx(signing_key, message, ctx)?
    } else {
        signing_key
            .try_sign(message)
            .map_err(|_| CryptoError::InternalError)?
    };

    Ok(SignatureBytes::new(
        signature.to_bytes().to_vec(),
        SignatureAlgorithm::Ed25519,
        SignatureEncoding::Raw,
    ))
}

fn verify_with_variant(
    verifying_key: &VerifyingKey,
    message: &[u8],
    signature: &[u8],
    variant: &Ed25519Variant,
) -> Result<()> {
    variant.validate()?;
    let sig = signature_from_bytes(signature)?;

    if variant.prehash {
        let mut hasher = Sha512::new();
        hasher.update(message);
        verifying_key
            .verify_prehashed_strict(hasher, variant.context.as_deref(), &sig)
            .map_err(|_| CryptoError::VerificationFailed)?;
    } else if let Some(ctx) = variant.context.as_deref() {
        verify_ed25519ctx(verifying_key, message, &sig, ctx)?;
    } else {
        verifying_key
            .verify_strict(message, &sig)
            .map_err(|_| CryptoError::VerificationFailed)?;
    }
    Ok(())
}

pub fn ed25519_sign(seed: &[u8; ED25519_KEY_SIZE_BYTES], message: &[u8]) -> Result<SignatureBytes> {
    let signing_key = SigningKey::from_bytes(seed);
    sign_with_variant(&signing_key, message, &Ed25519Variant::pure())
}

pub fn ed25519_sign_variant(
    seed: &[u8; ED25519_KEY_SIZE_BYTES],
    message: &[u8],
    variant: &Ed25519Variant,
) -> Result<SignatureBytes> {
    let signing_key = SigningKey::from_bytes(seed);
    sign_with_variant(&signing_key, message, variant)
}

pub fn ed25519_verify(
    public_key: &[u8; ED25519_KEY_SIZE_BYTES],
    message: &[u8],
    signature: &[u8],
) -> Result<()> {
    let verifying_key = verifying_key_from_bytes(public_key)?;
    verify_with_variant(&verifying_key, message, signature, &Ed25519Variant::pure())
}

pub fn ed25519_verify_variant(
    public_key: &[u8; ED25519_KEY_SIZE_BYTES],
    message: &[u8],
    signature: &[u8],
    variant: &Ed25519Variant,
) -> Result<()> {
    let verifying_key = verifying_key_from_bytes(public_key)?;
    verify_with_variant(&verifying_key, message, signature, variant)
}

pub fn ed25519_generate_keypair(rng: &mut dyn CryptoRng) -> Result<([u8; 32], [u8; 32])> {
    let mut seed = [0u8; ED25519_KEY_SIZE_BYTES];
    rng.fill_bytes(&mut seed);
    let signing_key = SigningKey::from_bytes(&seed);
    Ok((
        signing_key.to_bytes(),
        signing_key.verifying_key().to_bytes(),
    ))
}
