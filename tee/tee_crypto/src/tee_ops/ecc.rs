// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Configurable ECC operations with multiple curves and hash algorithms.
//!
//! Supports ECDSA sign/verify with P-192, P-224, P-256, P-384, P-521 and
//! configurable hash (SHA-1/224/256/384/512), plus ECDH key agreement and SM2
//! delegation. The caller is responsible for hashing the message before calling
//! sign/verify — the input `hash` parameter is the pre-hashed digest.

use alloc::vec::Vec;

use ecdsa::hazmat::{bits2field, sign_prehashed_rfc6979, verify_prehashed};
use elliptic_curve::{Generate, NonZeroScalar, sec1::FromSec1Point};

pub use crate::asymmetric::EccCurve;
use crate::{
    bytes::{BigEndianBytes, SecretBytes},
    error::{CryptoError, Result},
    hash::{DigestBytes, HashAlgorithm},
    material::{
        SharedSecretAlgorithm, SharedSecretBytes, SignatureAlgorithm, SignatureBytes,
        SignatureEncoding,
    },
    rng::CryptoRng,
};

/// Hash algorithm for ECDSA.
///
/// The `Sha1` variant is retained for `TEE_ALG_ECDSA_SHA1` compatibility.
/// SHA-1 is deprecated and weak; prefer SHA-256 or stronger for new use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EccHashAlgo {
    /// Legacy ECDSA hash — SHA-1; not recommended.
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sm3,
}

impl EccHashAlgo {
    pub const fn hash_algorithm(self) -> HashAlgorithm {
        match self {
            Self::Sha1 => HashAlgorithm::Sha1,
            Self::Sha224 => HashAlgorithm::Sha224,
            Self::Sha256 => HashAlgorithm::Sha256,
            Self::Sha384 => HashAlgorithm::Sha384,
            Self::Sha512 => HashAlgorithm::Sha512,
            Self::Sm3 => HashAlgorithm::Sm3,
        }
    }
}

impl From<EccHashAlgo> for HashAlgorithm {
    fn from(value: EccHashAlgo) -> Self {
        value.hash_algorithm()
    }
}

impl TryFrom<HashAlgorithm> for EccHashAlgo {
    type Error = CryptoError;

    fn try_from(value: HashAlgorithm) -> Result<Self> {
        match value {
            HashAlgorithm::Sha1 => Ok(Self::Sha1),
            HashAlgorithm::Sha224 => Ok(Self::Sha224),
            HashAlgorithm::Sha256 => Ok(Self::Sha256),
            HashAlgorithm::Sha384 => Ok(Self::Sha384),
            HashAlgorithm::Sha512 => Ok(Self::Sha512),
            HashAlgorithm::Sm3 => Ok(Self::Sm3),
            HashAlgorithm::Md5 => Err(CryptoError::UnsupportedAlgorithm),
        }
    }
}

/// Raw ECC keypair material in big-endian byte form.
pub struct EccKeypairBytes {
    pub private_key: SecretBytes,
    pub public_x: BigEndianBytes,
    pub public_y: BigEndianBytes,
}

/// Raw ECC public key material in big-endian byte form.
pub struct EccPublicKeyBytes {
    pub public_x: BigEndianBytes,
    pub public_y: BigEndianBytes,
}

/// Generate an ECC keypair and return typed raw byte components.
pub fn ecc_keygen_bytes(curve: EccCurve, rng: &mut dyn CryptoRng) -> Result<EccKeypairBytes> {
    match curve {
        EccCurve::P192 => {
            let sk = p192::NonZeroScalar::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = (p192::ProjectivePoint::GENERATOR * *sk).to_affine();
            let (x, y) = affine_to_xy::<p192::NistP192>(&pk)?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P224 => {
            let sk = p224::SecretKey::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p224::NistP224>(pk.as_affine())?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P256 => {
            let sk = p256::SecretKey::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p256::NistP256>(pk.as_affine())?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P384 => {
            let sk = p384::SecretKey::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p384::NistP384>(pk.as_affine())?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P521 => {
            let sk = p521::SecretKey::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p521::NistP521>(pk.as_affine())?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::Sm2 => {
            let sk = sm2::SecretKey::try_generate_from_rng(rng)
                .map_err(|_| CryptoError::InternalError)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<sm2::Sm2>(pk.as_affine())?;
            Ok(EccKeypairBytes {
                private_key: SecretBytes::new(sk.to_bytes().to_vec()),
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
    }
}

/// Generate an ECC keypair, returning (private_key_bytes, public_x, public_y).
pub fn ecc_keygen(curve: EccCurve, rng: &mut dyn CryptoRng) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let keypair = ecc_keygen_bytes(curve, rng)?;
    Ok((
        keypair.private_key.expose_secret_clone(),
        keypair.public_x.into_vec(),
        keypair.public_y.into_vec(),
    ))
}

/// ECDSA sign a pre-hashed message.
///
/// `hash` must be the output of the hash algorithm matching `hash_algo`.
/// Returns the raw `r||s` signature (OP-TEE format).
pub fn ecc_sign(
    curve: EccCurve,
    hash_algo: EccHashAlgo,
    secret_key: &[u8],
    hash: &DigestBytes,
    rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    validate_digest(hash, hash_algo)?;
    match curve {
        EccCurve::P192 => ecc_sign_p192(secret_key, hash.as_bytes(), rng),
        EccCurve::P224 => ecc_sign_p224(secret_key, hash.as_bytes(), rng),
        EccCurve::P256 => ecc_sign_p256(secret_key, hash.as_bytes(), rng),
        EccCurve::P384 => ecc_sign_p384(secret_key, hash.as_bytes(), rng),
        EccCurve::P521 => ecc_sign_p521(secret_key, hash.as_bytes(), rng),
        EccCurve::Sm2 => Err(CryptoError::UnsupportedAlgorithm),
    }
}

/// ECDSA verify a pre-hashed message.
///
/// `hash` must be the output of the hash algorithm matching `hash_algo`.
/// `signature` may be raw `r||s` or DER-encoded.
pub fn ecc_verify(
    curve: EccCurve,
    hash_algo: EccHashAlgo,
    public_x: &[u8],
    public_y: &[u8],
    hash: &DigestBytes,
    signature: &SignatureBytes,
) -> Result<()> {
    validate_digest(hash, hash_algo)?;
    if signature.algorithm() != SignatureAlgorithm::Ecdsa(curve) {
        return Err(CryptoError::AlgorithmMismatch);
    }
    match signature.encoding() {
        SignatureEncoding::Raw | SignatureEncoding::Der => {}
    }
    match curve {
        EccCurve::P192 => {
            ecc_verify_p192(public_x, public_y, hash.as_bytes(), signature.as_bytes())
        }
        EccCurve::P224 => {
            ecc_verify_p224(public_x, public_y, hash.as_bytes(), signature.as_bytes())
        }
        EccCurve::P256 => {
            ecc_verify_p256(public_x, public_y, hash.as_bytes(), signature.as_bytes())
        }
        EccCurve::P384 => {
            ecc_verify_p384(public_x, public_y, hash.as_bytes(), signature.as_bytes())
        }
        EccCurve::P521 => {
            ecc_verify_p521(public_x, public_y, hash.as_bytes(), signature.as_bytes())
        }
        EccCurve::Sm2 => Err(CryptoError::UnsupportedAlgorithm),
    }
}

fn validate_digest(digest: &DigestBytes, hash_algo: EccHashAlgo) -> Result<()> {
    let expected = hash_algo.hash_algorithm();
    if digest.algorithm() != expected {
        return Err(CryptoError::InvalidDigestAlgorithm);
    }
    // OP-TEE/xtest passes a caller-supplied prehash (regression_4006 uses SHA1 for
    // all ECDSA curves). bits2field() truncates to the curve order — do not require
    // digest.len() == hash output size (unlike standalone tee_crypto self-tests).
    if digest.as_bytes().is_empty() {
        return Err(CryptoError::InvalidLength);
    }
    Ok(())
}

/// Compute ECDH shared secret from local private key and peer's public key.
pub fn ecc_shared_secret(
    curve: EccCurve,
    local_secret: &[u8],
    peer_x: &[u8],
    peer_y: &[u8],
) -> Result<SharedSecretBytes> {
    match curve {
        EccCurve::P192 => ecdh_p192(local_secret, peer_x, peer_y),
        EccCurve::P224 => ecdh_p224(local_secret, peer_x, peer_y),
        EccCurve::P256 => ecdh_p256(local_secret, peer_x, peer_y),
        EccCurve::P384 => ecdh_p384(local_secret, peer_x, peer_y),
        EccCurve::P521 => ecdh_p521(local_secret, peer_x, peer_y),
        EccCurve::Sm2 => ecdh_sm2(local_secret, peer_x, peer_y),
    }
}

/// Derive typed public key bytes from private key bytes.
pub fn ecc_public_from_private_bytes(curve: EccCurve, secret: &[u8]) -> Result<EccPublicKeyBytes> {
    match curve {
        EccCurve::P192 => {
            let sk = nonzero_scalar_from_slice::<p192::NistP192>(secret)?;
            let pk = (p192::ProjectivePoint::GENERATOR * *sk).to_affine();
            let (x, y) = affine_to_xy::<p192::NistP192>(&pk)?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P224 => {
            let sk = p224::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p224::NistP224>(pk.as_affine())?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P256 => {
            let sk = p256::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p256::NistP256>(pk.as_affine())?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P384 => {
            let sk = p384::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p384::NistP384>(pk.as_affine())?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::P521 => {
            let sk = p521::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<p521::NistP521>(pk.as_affine())?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
        EccCurve::Sm2 => {
            let sk = sm2::SecretKey::from_slice(secret).map_err(|_| CryptoError::InvalidKey)?;
            let pk = sk.public_key();
            let (x, y) = affine_to_xy::<sm2::Sm2>(pk.as_affine())?;
            Ok(EccPublicKeyBytes {
                public_x: BigEndianBytes::new(x),
                public_y: BigEndianBytes::new(y),
            })
        }
    }
}

/// Derive public key (x, y) from private key bytes.
pub fn ecc_public_from_private(curve: EccCurve, secret: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let public = ecc_public_from_private_bytes(curve, secret)?;
    Ok((public.public_x.into_vec(), public.public_y.into_vec()))
}

fn affine_to_xy<C>(affine: &C::AffinePoint) -> Result<(Vec<u8>, Vec<u8>)>
where
    C: elliptic_curve::CurveArithmetic,
{
    use elliptic_curve::point::AffineCoordinates;
    Ok((affine.x().to_vec(), affine.y().to_vec()))
}

fn public_from_xy<C>(x: &[u8], y: &[u8]) -> Result<elliptic_curve::PublicKey<C>>
where
    C: elliptic_curve::CurveArithmetic,
    C::AffinePoint: FromSec1Point<C>,
    elliptic_curve::FieldBytesSize<C>: elliptic_curve::sec1::ModulusSize,
{
    let x_bytes: &elliptic_curve::FieldBytes<C> =
        x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &elliptic_curve::FieldBytes<C> =
        y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = C::AffinePoint::from_sec1_point(
        &elliptic_curve::sec1::Sec1Point::<C>::from_affine_coordinates(x_bytes, y_bytes, false),
    )
    .into_option()
    .ok_or(CryptoError::InvalidKey)?;
    elliptic_curve::PublicKey::<C>::from_affine(point).map_err(|_| CryptoError::InvalidKey)
}

fn nonzero_scalar_from_slice<C>(secret: &[u8]) -> Result<NonZeroScalar<C>>
where
    C: elliptic_curve::CurveArithmetic,
{
    let bytes: &elliptic_curve::FieldBytes<C> =
        secret.try_into().map_err(|_| CryptoError::InvalidLength)?;
    NonZeroScalar::<C>::from_repr(*bytes)
        .into_option()
        .ok_or(CryptoError::InvalidKey)
}

fn parse_p192_signature(signature: &[u8]) -> Result<ecdsa::Signature<p192::NistP192>> {
    if signature.len() == 48 {
        let bytes: &ecdsa::SignatureBytes<p192::NistP192> = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?;
        ecdsa::Signature::<p192::NistP192>::from_bytes(bytes).map_err(|_| CryptoError::InvalidInput)
    } else {
        ecdsa::Signature::<p192::NistP192>::from_der(signature)
            .map_err(|_| CryptoError::InvalidInput)
    }
}

fn parse_p224_signature(signature: &[u8]) -> Result<ecdsa::Signature<p224::NistP224>> {
    if signature.len() == 56 {
        let bytes: &ecdsa::SignatureBytes<p224::NistP224> = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?;
        ecdsa::Signature::<p224::NistP224>::from_bytes(bytes).map_err(|_| CryptoError::InvalidInput)
    } else {
        ecdsa::Signature::<p224::NistP224>::from_der(signature)
            .map_err(|_| CryptoError::InvalidInput)
    }
}

fn parse_p256_signature(signature: &[u8]) -> Result<ecdsa::Signature<p256::NistP256>> {
    if signature.len() == 64 {
        let bytes: &ecdsa::SignatureBytes<p256::NistP256> = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?;
        ecdsa::Signature::<p256::NistP256>::from_bytes(bytes).map_err(|_| CryptoError::InvalidInput)
    } else {
        ecdsa::Signature::<p256::NistP256>::from_der(signature)
            .map_err(|_| CryptoError::InvalidInput)
    }
}

fn parse_p384_signature(signature: &[u8]) -> Result<ecdsa::Signature<p384::NistP384>> {
    if signature.len() == 96 {
        let bytes: &ecdsa::SignatureBytes<p384::NistP384> = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?;
        ecdsa::Signature::<p384::NistP384>::from_bytes(bytes).map_err(|_| CryptoError::InvalidInput)
    } else {
        ecdsa::Signature::<p384::NistP384>::from_der(signature)
            .map_err(|_| CryptoError::InvalidInput)
    }
}

fn parse_p521_signature(signature: &[u8]) -> Result<ecdsa::Signature<p521::NistP521>> {
    if signature.len() == 132 {
        let bytes: &ecdsa::SignatureBytes<p521::NistP521> = signature
            .try_into()
            .map_err(|_| CryptoError::InvalidInput)?;
        ecdsa::Signature::<p521::NistP521>::from_bytes(bytes).map_err(|_| CryptoError::InvalidInput)
    } else {
        ecdsa::Signature::<p521::NistP521>::from_der(signature)
            .map_err(|_| CryptoError::InvalidInput)
    }
}

// --- P-192 helpers ---

fn ecc_sign_p192(
    secret_key: &[u8],
    hash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let d = nonzero_scalar_from_slice::<p192::NistP192>(secret_key)?;
    let z = bits2field::<p192::NistP192>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let (sig, _) = sign_prehashed_rfc6979::<p192::NistP192, sha1::Sha1>(&d, &z, &[])
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().as_slice().to_vec(),
        SignatureAlgorithm::Ecdsa(EccCurve::P192),
        SignatureEncoding::Raw,
    ))
}

fn ecc_verify_p192(public_x: &[u8], public_y: &[u8], hash: &[u8], signature: &[u8]) -> Result<()> {
    let pk = public_from_xy::<p192::NistP192>(public_x, public_y)?;
    let z = bits2field::<p192::NistP192>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let sig = parse_p192_signature(signature)?;
    verify_prehashed::<p192::NistP192>(&pk.as_affine().into(), &z, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

fn ecdh_p192(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = nonzero_scalar_from_slice::<p192::NistP192>(local_secret)?;
    let peer_pk = public_from_xy::<p192::NistP192>(peer_x, peer_y)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(&sk, peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Ecdh(EccCurve::P192),
    ))
}

// --- P-224 helpers ---

fn ecc_sign_p224(
    secret_key: &[u8],
    hash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = p224::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let d = NonZeroScalar::<p224::NistP224>::from_repr(sk.to_bytes())
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let z = bits2field::<p224::NistP224>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let (sig, _) = sign_prehashed_rfc6979::<p224::NistP224, sha2::Sha224>(&d, &z, &[])
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().as_slice().to_vec(),
        SignatureAlgorithm::Ecdsa(EccCurve::P224),
        SignatureEncoding::Raw,
    ))
}

fn ecc_verify_p224(public_x: &[u8], public_y: &[u8], hash: &[u8], signature: &[u8]) -> Result<()> {
    let pk = public_from_xy::<p224::NistP224>(public_x, public_y)?;
    let z = bits2field::<p224::NistP224>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let sig = parse_p224_signature(signature)?;
    verify_prehashed::<p224::NistP224>(&pk.as_affine().into(), &z, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

fn ecdh_p224(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = p224::SecretKey::from_slice(local_secret).map_err(|_| CryptoError::InvalidKey)?;
    let peer_pk = public_from_xy::<p224::NistP224>(peer_x, peer_y)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Ecdh(EccCurve::P224),
    ))
}

// --- P-256 helpers ---

fn ecc_sign_p256(
    secret_key: &[u8],
    hash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = p256::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let d = NonZeroScalar::<p256::NistP256>::from_repr(sk.to_bytes())
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let z = bits2field::<p256::NistP256>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let (sig, _) = sign_prehashed_rfc6979::<p256::NistP256, sha2::Sha256>(&d, &z, &[])
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().as_slice().to_vec(),
        SignatureAlgorithm::Ecdsa(EccCurve::P256),
        SignatureEncoding::Raw,
    ))
}

fn ecc_verify_p256(public_x: &[u8], public_y: &[u8], hash: &[u8], signature: &[u8]) -> Result<()> {
    let pk = public_from_xy::<p256::NistP256>(public_x, public_y)?;
    let z = bits2field::<p256::NistP256>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let sig = parse_p256_signature(signature)?;
    verify_prehashed::<p256::NistP256>(&pk.as_affine().into(), &z, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

fn ecdh_p256(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = p256::SecretKey::from_slice(local_secret).map_err(|_| CryptoError::InvalidKey)?;
    let peer_pk = public_from_xy::<p256::NistP256>(peer_x, peer_y)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Ecdh(EccCurve::P256),
    ))
}

// --- P-384 helpers ---

fn ecc_sign_p384(
    secret_key: &[u8],
    hash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = p384::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let d = NonZeroScalar::<p384::NistP384>::from_repr(sk.to_bytes())
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let z = bits2field::<p384::NistP384>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let (sig, _) = sign_prehashed_rfc6979::<p384::NistP384, sha2::Sha384>(&d, &z, &[])
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().as_slice().to_vec(),
        SignatureAlgorithm::Ecdsa(EccCurve::P384),
        SignatureEncoding::Raw,
    ))
}

fn ecc_verify_p384(public_x: &[u8], public_y: &[u8], hash: &[u8], signature: &[u8]) -> Result<()> {
    let pk = public_from_xy::<p384::NistP384>(public_x, public_y)?;
    let z = bits2field::<p384::NistP384>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let sig = parse_p384_signature(signature)?;
    verify_prehashed::<p384::NistP384>(&pk.as_affine().into(), &z, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

fn ecdh_p384(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = p384::SecretKey::from_slice(local_secret).map_err(|_| CryptoError::InvalidKey)?;
    let peer_pk = public_from_xy::<p384::NistP384>(peer_x, peer_y)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Ecdh(EccCurve::P384),
    ))
}

// --- P-521 helpers ---

fn ecc_sign_p521(
    secret_key: &[u8],
    hash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = p521::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let d = NonZeroScalar::<p521::NistP521>::from_repr(sk.to_bytes())
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let z = bits2field::<p521::NistP521>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let (sig, _) = sign_prehashed_rfc6979::<p521::NistP521, sha2::Sha512>(&d, &z, &[])
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().as_slice().to_vec(),
        SignatureAlgorithm::Ecdsa(EccCurve::P521),
        SignatureEncoding::Raw,
    ))
}

fn ecc_verify_p521(public_x: &[u8], public_y: &[u8], hash: &[u8], signature: &[u8]) -> Result<()> {
    let pk = public_from_xy::<p521::NistP521>(public_x, public_y)?;
    let z = bits2field::<p521::NistP521>(hash).map_err(|_| CryptoError::InvalidInput)?;
    let sig = parse_p521_signature(signature)?;
    verify_prehashed::<p521::NistP521>(&pk.as_affine().into(), &z, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

fn ecdh_p521(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = p521::SecretKey::from_slice(local_secret).map_err(|_| CryptoError::InvalidKey)?;
    let peer_pk = public_from_xy::<p521::NistP521>(peer_x, peer_y)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Ecdh(EccCurve::P521),
    ))
}

// --- SM2 helpers ---

fn ecdh_sm2(local_secret: &[u8], peer_x: &[u8], peer_y: &[u8]) -> Result<SharedSecretBytes> {
    let sk = sm2::SecretKey::from_slice(local_secret).map_err(|_| CryptoError::InvalidKey)?;
    let x_bytes: &sm2::FieldBytes = peer_x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = peer_y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    let affine = sm2::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let peer_pk = sm2::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)?;
    let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), peer_pk.as_affine());
    Ok(SharedSecretBytes::new(
        shared.raw_secret_bytes().to_vec(),
        SharedSecretAlgorithm::Sm2Kep,
    ))
}

/// Parse a 32-byte P-256 ECDSA private scalar from PKCS#8 DER.
pub fn p256_secret_scalar_from_pkcs8_der(der: &[u8]) -> Result<[u8; 32]> {
    use elliptic_curve::pkcs8::DecodePrivateKey;
    let sk = p256::ecdsa::SigningKey::from_pkcs8_der(der).map_err(|_| CryptoError::InvalidKey)?;
    Ok(sk.to_bytes().into())
}

/// Extract P-256 public point coordinates from SPKI DER.
pub fn p256_public_xy_from_spki_der(spki_der: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    use elliptic_curve::{pkcs8::DecodePublicKey, sec1::ToSec1Point};
    let pk = p256::PublicKey::from_public_key_der(spki_der).map_err(|_| CryptoError::InvalidKey)?;
    let point = pk.as_affine().to_sec1_point(false);
    let x = point.x().ok_or(CryptoError::InvalidKey)?.to_vec();
    let y = point.y().ok_or(CryptoError::InvalidKey)?.to_vec();
    Ok((x, y))
}

/// Sign a pre-hashed P-256 ECDSA digest; returns DER-encoded signature (CMS detached).
pub fn ecc_sign_p256_prehash_der(
    secret_key: &[u8],
    digest: &[u8; 32],
    rng: &mut dyn CryptoRng,
) -> Result<Vec<u8>> {
    let sig = ecc_sign_p256(secret_key, digest, rng)?;
    let raw = sig.as_bytes();
    let sig = p256::ecdsa::Signature::from_slice(raw).map_err(|_| CryptoError::InvalidInput)?;
    Ok(sig.to_der().as_bytes().to_vec())
}
