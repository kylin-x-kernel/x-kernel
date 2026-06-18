// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ECC implementation (P-256, P-384, P-521) using the RustCrypto crates.
//!
//! Supports key generation, sign/verify (ECDSA), and key agreement (ECDH)
//! for NIST P-256, P-384, and P-521 curves.

use ecdsa::signature::{Signer as EcdsaSigner, Verifier as EcdsaVerifier};
use elliptic_curve::sec1::{FromSec1Point, ToSec1Point};

use crate::{
    asymmetric::{
        EccCurve, EccPublicPoint, KeyAgreement, Keypair, PublicKeyComponents, Signer, Verifier,
    },
    error::{CryptoError, Result},
    material::{
        SharedSecretAlgorithm, SharedSecretBytes, SignatureAlgorithm, SignatureBytes,
        SignatureEncoding,
    },
    rng::CryptoRng,
};

macro_rules! impl_ecc_keypair {
    (
        $(#[$meta:meta])*
        $name:ident, $curve:ty, $curve_id:path
    ) => {
        $(#[$meta])*
        pub struct $name {
            signing_key: ecdsa::SigningKey<$curve>,
        }

        impl $name {
            /// Access the inner signing key.
            pub fn as_inner(&self) -> &ecdsa::SigningKey<$curve> {
                &self.signing_key
            }

            /// Derive the verifying key.
            pub fn verifying_key(&self) -> &ecdsa::VerifyingKey<$curve> {
                self.signing_key.verifying_key()
            }
        }

        impl Keypair for $name {
            fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
                use elliptic_curve::Generate;
                let signing_key = ecdsa::SigningKey::<$curve>::try_generate_from_rng(rng)
                    .map_err(|_| CryptoError::InternalError)?;
                Ok(Self { signing_key })
            }

            fn to_public_components(&self) -> Result<PublicKeyComponents> {
                let point = self.verifying_key().as_affine().to_sec1_point(false);
                let (x, y) = match point.coordinates() {
                    elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
                    _ => return Err(CryptoError::InternalError),
                };
                Ok(PublicKeyComponents::Ecc(EccPublicPoint::from_be_bytes(
                    $curve_id,
                    x.to_vec(),
                    y.to_vec(),
                )))
            }
        }

        impl Signer for $name {
            fn sign(&self, msg: &[u8], _rng: &mut dyn CryptoRng) -> Result<SignatureBytes> {
                let sig: ecdsa::Signature<$curve> = self.signing_key.sign(msg);
                Ok(SignatureBytes::new(
                    sig.to_bytes().to_vec(),
                    SignatureAlgorithm::Ecdsa($curve_id),
                    SignatureEncoding::Raw,
                ))
            }
        }

        impl Verifier for $name {
            fn verify(&self, msg: &[u8], signature: &SignatureBytes) -> Result<()> {
                if signature.algorithm() != SignatureAlgorithm::Ecdsa($curve_id) {
                    return Err(CryptoError::InvalidInput);
                }
                if signature.encoding() != SignatureEncoding::Raw {
                    return Err(CryptoError::InvalidInput);
                }
                let signature = ecdsa::Signature::<$curve>::from_slice(signature.as_bytes())
                    .map_err(|_| CryptoError::InvalidInput)?;
                self.verifying_key()
                    .verify(msg, &signature)
                    .map_err(|_| CryptoError::VerificationFailed)
            }
        }
    };
}

impl_ecc_keypair!(
    /// NIST P-256 ECDSA keypair.
    EccP256Keypair, p256::NistP256, EccCurve::P256
);

impl_ecc_keypair!(
    /// NIST P-384 ECDSA keypair.
    EccP384Keypair, p384::NistP384, EccCurve::P384
);

impl_ecc_keypair!(
    /// NIST P-521 ECDSA keypair.
    EccP521Keypair, p521::NistP521, EccCurve::P521
);

/// Parse x, y bytes into a p256 PublicKey.
fn p256_public_key_from_xy(x: &[u8], y: &[u8]) -> Result<p256::PublicKey> {
    let x_bytes: &p256::FieldBytes = x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &p256::FieldBytes = y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = p256::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    p256::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)
        .and_then(|affine| {
            p256::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)
        })
}

/// Parse x, y bytes into a p384 PublicKey.
fn p384_public_key_from_xy(x: &[u8], y: &[u8]) -> Result<p384::PublicKey> {
    let x_bytes: &p384::FieldBytes = x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &p384::FieldBytes = y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = p384::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    p384::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)
        .and_then(|affine| {
            p384::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)
        })
}

/// Parse x, y bytes into a p521 PublicKey.
fn p521_public_key_from_xy(x: &[u8], y: &[u8]) -> Result<p521::PublicKey> {
    let x_bytes: &p521::FieldBytes = x.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &p521::FieldBytes = y.try_into().map_err(|_| CryptoError::InvalidLength)?;
    let point = p521::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    p521::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)
        .and_then(|affine| {
            p521::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)
        })
}

impl KeyAgreement for EccP256Keypair {
    fn shared_secret(&self, peer_public: &PublicKeyComponents) -> Result<SharedSecretBytes> {
        let point = match peer_public {
            PublicKeyComponents::Ecc(point) if point.curve() == EccCurve::P256 => point,
            PublicKeyComponents::Ecc(_) => return Err(CryptoError::InvalidInput),
            _ => return Err(CryptoError::InvalidInput),
        };
        let peer_pk = p256_public_key_from_xy(point.x(), point.y())?;
        let secret = elliptic_curve::ecdh::diffie_hellman(
            self.signing_key.as_nonzero_scalar(),
            peer_pk.as_affine(),
        );
        Ok(SharedSecretBytes::new(
            secret.raw_secret_bytes().to_vec(),
            SharedSecretAlgorithm::Ecdh(EccCurve::P256),
        ))
    }
}

impl KeyAgreement for EccP384Keypair {
    fn shared_secret(&self, peer_public: &PublicKeyComponents) -> Result<SharedSecretBytes> {
        let point = match peer_public {
            PublicKeyComponents::Ecc(point) if point.curve() == EccCurve::P384 => point,
            PublicKeyComponents::Ecc(_) => return Err(CryptoError::InvalidInput),
            _ => return Err(CryptoError::InvalidInput),
        };
        let peer_pk = p384_public_key_from_xy(point.x(), point.y())?;
        let secret = elliptic_curve::ecdh::diffie_hellman(
            self.signing_key.as_nonzero_scalar(),
            peer_pk.as_affine(),
        );
        Ok(SharedSecretBytes::new(
            secret.raw_secret_bytes().to_vec(),
            SharedSecretAlgorithm::Ecdh(EccCurve::P384),
        ))
    }
}

impl KeyAgreement for EccP521Keypair {
    fn shared_secret(&self, peer_public: &PublicKeyComponents) -> Result<SharedSecretBytes> {
        let point = match peer_public {
            PublicKeyComponents::Ecc(point) if point.curve() == EccCurve::P521 => point,
            PublicKeyComponents::Ecc(_) => return Err(CryptoError::InvalidInput),
            _ => return Err(CryptoError::InvalidInput),
        };
        let peer_pk = p521_public_key_from_xy(point.x(), point.y())?;
        let secret = elliptic_curve::ecdh::diffie_hellman(
            self.signing_key.as_nonzero_scalar(),
            peer_pk.as_affine(),
        );
        Ok(SharedSecretBytes::new(
            secret.raw_secret_bytes().to_vec(),
            SharedSecretAlgorithm::Ecdh(EccCurve::P521),
        ))
    }
}
