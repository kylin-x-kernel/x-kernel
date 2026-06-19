// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SM2 implementation (DSA, PKE, KEP) using the `sm2` crate.
//!
//! Supports:
//! - SM2 DSA: digital signature (sign/verify with SM3)
//! - SM2 PKE: public key encryption/decryption (encrypt/decrypt with SM3)
//! - SM2 KEP: key exchange protocol (ECDH + SM3 KDF)

use elliptic_curve::sec1::{FromSec1Point, ToSec1Point};
use signature::hazmat::{PrehashSigner, PrehashVerifier};
use sm3::{Digest, Sm3};

use crate::{
    asymmetric::{
        Decryptor, Encryptor, KeyAgreement, Keypair, PublicKeyComponents, Signer, Sm2PublicPoint,
        Verifier,
    },
    bytes::PlaintextBytes,
    error::{CryptoError, Result},
    material::{
        CiphertextAlgorithm, CiphertextBytes, SharedSecretAlgorithm, SharedSecretBytes,
        SignatureAlgorithm, SignatureBytes, SignatureEncoding,
    },
    rng::{CryptoRng, RngAdapter},
};

/// Default distinguishing identifier for SM2 DSA.
const DEFAULT_DISTID: &str = "1234567812345678";

/// GM/T 0003 recommended curve parameters for ZA = SM3(ENTLA || ID || a || b || Gx || Gy || Px || Py).
const SM2_EQUATION_A: [u8; 32] = [
    0xff, 0xff, 0xff, 0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfc,
];
const SM2_EQUATION_B: [u8; 32] = [
    0x28, 0xe9, 0xfa, 0x9e, 0x9d, 0x9f, 0x5e, 0x34, 0x4d, 0x5a, 0x9e, 0x4b, 0xcf, 0x65, 0x09, 0xa7,
    0xf3, 0x97, 0x89, 0xf5, 0x15, 0xab, 0x8f, 0x92, 0xdd, 0xbc, 0xbd, 0x41, 0x4d, 0x94, 0x0e, 0x93,
];
const SM2_GENERATOR_X: [u8; 32] = [
    0x32, 0xc4, 0xae, 0x2c, 0x1f, 0x19, 0x81, 0x19, 0x5f, 0x99, 0x04, 0x46, 0x6a, 0x39, 0xc9, 0x94,
    0x8f, 0xe3, 0x0b, 0xbf, 0xf2, 0x66, 0x0b, 0xe1, 0x71, 0x5a, 0x45, 0x89, 0x33, 0x4c, 0x74, 0xc7,
];
const SM2_GENERATOR_Y: [u8; 32] = [
    0xbc, 0x37, 0x36, 0xa2, 0xf4, 0xf6, 0x77, 0x9c, 0x59, 0xbd, 0xce, 0xe3, 0x6b, 0x69, 0x21, 0x53,
    0xd0, 0xa9, 0x87, 0x7c, 0xc6, 0x2a, 0x47, 0x40, 0x02, 0xdf, 0x32, 0xe5, 0x21, 0x39, 0xf0, 0xa0,
];

/// SM2 Digital Signature Algorithm keypair.
///
/// Wraps `sm2::dsa::SigningKey` for signing and verification.
/// Uses SM3 as the hash function internally.
pub struct Sm2DsaKeypair {
    signing_key: sm2::dsa::SigningKey,
}

impl Sm2DsaKeypair {
    /// Access the inner signing key.
    pub fn as_inner(&self) -> &sm2::dsa::SigningKey {
        &self.signing_key
    }

    /// Get the verifying key.
    pub fn verifying_key(&self) -> &sm2::dsa::VerifyingKey {
        self.signing_key.verifying_key()
    }
}

impl Keypair for Sm2DsaKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        let distid = DEFAULT_DISTID;
        let signing_key = sm2::dsa::SigningKey::new(distid, &secret_key)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(Self { signing_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let point = self.verifying_key().as_affine().to_sec1_point(false);
        let (x, y) = match point.coordinates() {
            elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
            _ => return Err(CryptoError::InternalError),
        };
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

impl Signer for Sm2DsaKeypair {
    fn sign(&self, msg: &[u8], _rng: &mut dyn CryptoRng) -> Result<SignatureBytes> {
        use sm2::dsa::signature::Signer as _;
        let sig: sm2::dsa::Signature = self
            .signing_key
            .try_sign(msg)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(SignatureBytes::new(
            sig.to_bytes().to_vec(),
            SignatureAlgorithm::Sm2Dsa,
            SignatureEncoding::Raw,
        ))
    }
}

impl Verifier for Sm2DsaKeypair {
    fn verify(&self, msg: &[u8], signature: &SignatureBytes) -> Result<()> {
        if signature.algorithm() != SignatureAlgorithm::Sm2Dsa {
            return Err(CryptoError::InvalidInput);
        }
        if signature.encoding() != SignatureEncoding::Raw {
            return Err(CryptoError::InvalidInput);
        }
        use sm2::dsa::signature::Verifier;
        let signature = sm2::dsa::Signature::from_slice(signature.as_bytes())
            .map_err(|_| CryptoError::InvalidInput)?;
        self.verifying_key()
            .verify(msg, &signature)
            .map_err(|_| CryptoError::VerificationFailed)
    }
}

/// SM2 Public Key Encryption keypair.
///
/// Wraps `sm2::pke::DecryptingKey` and its associated `EncryptingKey`.
pub struct Sm2PkeKeypair {
    decrypting_key: sm2::pke::DecryptingKey,
}

impl Sm2PkeKeypair {
    /// Access the inner decrypting key.
    pub fn as_inner(&self) -> &sm2::pke::DecryptingKey {
        &self.decrypting_key
    }

    /// Get the encrypting key.
    pub fn encrypting_key(&self) -> &sm2::pke::EncryptingKey {
        self.decrypting_key.encrypting_key()
    }
}

impl Keypair for Sm2PkeKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        let decrypting_key = sm2::pke::DecryptingKey::new(secret_key);
        Ok(Self { decrypting_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let point = self.encrypting_key().as_affine().to_sec1_point(false);
        let (x, y) = match point.coordinates() {
            elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
            _ => return Err(CryptoError::InternalError),
        };
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

impl Encryptor for Sm2PkeKeypair {
    fn encrypt(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<CiphertextBytes> {
        let mut rng = RngAdapter::new(rng);
        let ciphertext = self
            .encrypting_key()
            .encrypt(&mut rng, msg)
            .map_err(|_| CryptoError::InternalError)?;
        Ok(CiphertextBytes::new(
            ciphertext,
            CiphertextAlgorithm::Sm2Pke,
        ))
    }
}

impl Decryptor for Sm2PkeKeypair {
    fn decrypt(&self, ciphertext: &CiphertextBytes) -> Result<PlaintextBytes> {
        if ciphertext.algorithm() != CiphertextAlgorithm::Sm2Pke {
            return Err(CryptoError::InvalidInput);
        }
        let plaintext = self
            .decrypting_key
            .decrypt(ciphertext.as_bytes())
            .map_err(|_| CryptoError::InternalError)?;
        Ok(PlaintextBytes::new(plaintext))
    }
}

/// SM2 Key Exchange Protocol keypair.
///
/// Implements SM2 key exchange using ECDH shared secret + SM3 KDF,
/// following GM/T 0003.3-2012.
pub struct Sm2KepKeypair {
    secret_key: sm2::SecretKey,
}

impl Keypair for Sm2KepKeypair {
    fn generate(rng: &mut dyn CryptoRng, _key_size_bits: usize) -> Result<Self> {
        use elliptic_curve::Generate;
        let secret_key =
            sm2::SecretKey::try_generate_from_rng(rng).map_err(|_| CryptoError::InternalError)?;
        Ok(Self { secret_key })
    }

    fn to_public_components(&self) -> Result<PublicKeyComponents> {
        let public_key = self.secret_key.public_key();
        let point = public_key.as_affine().to_sec1_point(false);
        let (x, y) = match point.coordinates() {
            elliptic_curve::sec1::Coordinates::Uncompressed { x, y } => (x, y),
            _ => return Err(CryptoError::InternalError),
        };
        Ok(PublicKeyComponents::Sm2(Sm2PublicPoint::from_be_bytes(
            x.to_vec(),
            y.to_vec(),
        )))
    }
}

impl KeyAgreement for Sm2KepKeypair {
    fn shared_secret(&self, peer_public: &PublicKeyComponents) -> Result<SharedSecretBytes> {
        let point = match peer_public {
            PublicKeyComponents::Sm2(point) => point,
            _ => return Err(CryptoError::InvalidInput),
        };

        // Parse peer's public key
        let x_bytes: &sm2::FieldBytes = point
            .x()
            .try_into()
            .map_err(|_| CryptoError::InvalidLength)?;
        let y_bytes: &sm2::FieldBytes = point
            .y()
            .try_into()
            .map_err(|_| CryptoError::InvalidLength)?;
        let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
        let peer_affine = sm2::AffinePoint::from_sec1_point(&point)
            .into_option()
            .ok_or(CryptoError::InvalidKey)?;
        let peer_pk =
            sm2::PublicKey::from_affine(peer_affine).map_err(|_| CryptoError::InvalidKey)?;

        // ECDH: compute raw shared secret
        let shared = elliptic_curve::ecdh::diffie_hellman(
            self.secret_key.to_nonzero_scalar(),
            peer_pk.as_affine(),
        );

        // SM3 KDF: derive session key from raw shared secret
        let raw = shared.raw_secret_bytes();
        use digest::Digest;
        let mut hasher = sm3::Sm3::new();
        hasher.update(raw);
        let derived = hasher.finalize();

        Ok(SharedSecretBytes::new(
            derived.to_vec(),
            SharedSecretAlgorithm::Sm2Kep,
        ))
    }
}

/// SM2 PKE encrypt using raw public key (x, y) bytes.
pub fn sm2_pke_encrypt(
    public_x: &[u8],
    public_y: &[u8],
    input: &[u8],
    rng: &mut dyn CryptoRng,
) -> Result<CiphertextBytes> {
    let x_bytes: &sm2::FieldBytes = public_x
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = public_y
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    let affine = sm2::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let enc_key =
        sm2::pke::EncryptingKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)?;
    let mut rng = RngAdapter::new(rng);
    let ciphertext = enc_key
        .encrypt(&mut rng, input)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(CiphertextBytes::new(
        ciphertext,
        CiphertextAlgorithm::Sm2Pke,
    ))
}

/// SM2 PKE decrypt using raw private key bytes.
pub fn sm2_pke_decrypt(secret_key: &[u8], ciphertext: &CiphertextBytes) -> Result<PlaintextBytes> {
    if ciphertext.algorithm() != CiphertextAlgorithm::Sm2Pke {
        return Err(CryptoError::InvalidInput);
    }
    let sk = sm2::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let dk = sm2::pke::DecryptingKey::new(sk);
    let plaintext = dk
        .decrypt(ciphertext.as_bytes())
        .map_err(|_| CryptoError::InternalError)?;
    Ok(PlaintextBytes::new(plaintext))
}

/// SM2 DSA sign using raw private key bytes and a precomputed digest `e`.
///
/// OP-TEE/xtest pass `e = SM3(ZA || M)` (e.g. `SM3(ptx)` when `ptx = Z || M`),
/// not the raw message — use [`PrehashSigner::sign_prehash`], not `try_sign`.
pub fn sm2_dsa_sign(
    secret_key: &[u8],
    prehash: &[u8],
    _rng: &mut dyn CryptoRng,
) -> Result<SignatureBytes> {
    let sk = sm2::SecretKey::from_slice(secret_key).map_err(|_| CryptoError::InvalidKey)?;
    let distid = DEFAULT_DISTID;
    let signing_key =
        sm2::dsa::SigningKey::new(distid, &sk).map_err(|_| CryptoError::InternalError)?;
    let sig: sm2::dsa::Signature = signing_key
        .sign_prehash(prehash)
        .map_err(|_| CryptoError::InternalError)?;
    Ok(SignatureBytes::new(
        sig.to_bytes().to_vec(),
        SignatureAlgorithm::Sm2Dsa,
        SignatureEncoding::Raw,
    ))
}

/// SM2 DSA verify using raw public key (x, y) bytes and precomputed digest `e`.
///
/// Matches OP-TEE `sm2_verify_digest_raw`: `digest` is `e = SM3(ZA || M)`, signature
/// is raw 64-byte `r||s` or DER.
pub fn sm2_dsa_verify(
    public_x: &[u8],
    public_y: &[u8],
    prehash: &[u8],
    signature: &SignatureBytes,
) -> Result<()> {
    if signature.algorithm() != SignatureAlgorithm::Sm2Dsa {
        return Err(CryptoError::AlgorithmMismatch);
    }
    let x_bytes: &sm2::FieldBytes = public_x
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let y_bytes: &sm2::FieldBytes = public_y
        .try_into()
        .map_err(|_| CryptoError::InvalidLength)?;
    let point = sm2::Sec1Point::from_affine_coordinates(x_bytes, y_bytes, false);
    let affine = sm2::AffinePoint::from_sec1_point(&point)
        .into_option()
        .ok_or(CryptoError::InvalidKey)?;
    let pk = sm2::PublicKey::from_affine(affine).map_err(|_| CryptoError::InvalidKey)?;
    let distid = DEFAULT_DISTID;
    let verifying_key =
        sm2::dsa::VerifyingKey::new(distid, pk).map_err(|_| CryptoError::InternalError)?;
    let sig = match signature.encoding() {
        SignatureEncoding::Raw => sm2::dsa::Signature::from_slice(signature.as_bytes()),
        SignatureEncoding::Der => sm2::dsa::Signature::from_der(signature.as_bytes()),
    }
    .map_err(|_| CryptoError::InvalidInput)?;
    verifying_key
        .verify_prehash(prehash, &sig)
        .map_err(|_| CryptoError::VerificationFailed)
}

/// Compute SM2 DSA sign/verify digest `e = SM3(ZA || M)` (OP-TEE `sm2_compute_sign_digest`).
///
/// Callers of [`sm2_dsa_sign`] / [`sm2_dsa_verify`] must pass this value, not the raw message.
pub fn sm2_compute_sign_digest(
    distid: Option<&str>,
    message: &[u8],
    public_key: &[u8; 64],
) -> Result<[u8; 32]> {
    let distid = distid.unwrap_or(DEFAULT_DISTID);
    let entla: u16 = (distid.len() * 8)
        .try_into()
        .map_err(|_| CryptoError::InvalidInput)?;
    let mut hasher = Sm3::new();
    hasher.update(entla.to_be_bytes());
    hasher.update(distid.as_bytes());
    hasher.update(SM2_EQUATION_A);
    hasher.update(SM2_EQUATION_B);
    hasher.update(SM2_GENERATOR_X);
    hasher.update(SM2_GENERATOR_Y);
    hasher.update(&public_key[..32]);
    hasher.update(&public_key[32..]);
    let z: [u8; 32] = hasher.finalize().into();

    let mut hasher = Sm3::new();
    hasher.update(z);
    hasher.update(message);
    Ok(hasher.finalize().into())
}
