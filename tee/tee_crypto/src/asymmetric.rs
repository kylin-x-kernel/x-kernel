// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Asymmetric crypto traits and shared types.
//!
//! Defines the trait hierarchy and types used by RSA, ECC, and SM2 modules.

use alloc::vec::Vec;

use crate::{
    bytes::{BigEndianBytes, PlaintextBytes},
    error::Result,
    material::{CiphertextBytes, SharedSecretBytes, SignatureBytes},
    rng::CryptoRng,
};

/// ECC curve identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EccCurve {
    /// NIST P-192 (secp192r1)
    P192,
    /// NIST P-224 (secp224r1)
    P224,
    /// NIST P-256 (secp256r1)
    P256,
    /// NIST P-384 (secp384r1)
    P384,
    /// NIST P-521 (secp521r1)
    P521,
    /// SM2 curve (sm2p256v1)
    Sm2,
}

/// RSA padding mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RsaPadding {
    /// PKCS#1 v1.5 padding
    Pkcs1v15,
    /// Probabilistic Signature Scheme (PSS)
    Pss,
    /// Optimal Asymmetric Encryption Padding (OAEP)
    Oaep,
    /// No padding (raw/textbook RSA)
    None,
}

/// RSA public key components in big-endian byte form.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RsaPublicComponents {
    n: BigEndianBytes,
    e: BigEndianBytes,
}

impl RsaPublicComponents {
    pub fn new(n: BigEndianBytes, e: BigEndianBytes) -> Self {
        Self { n, e }
    }

    pub fn from_be_bytes(n: Vec<u8>, e: Vec<u8>) -> Self {
        Self::new(BigEndianBytes::new(n), BigEndianBytes::new(e))
    }

    pub fn n(&self) -> &[u8] {
        self.n.as_bytes()
    }

    pub fn e(&self) -> &[u8] {
        self.e.as_bytes()
    }

    pub fn into_parts(self) -> (BigEndianBytes, BigEndianBytes) {
        (self.n, self.e)
    }
}

/// ECC affine public point in big-endian byte form.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EccPublicPoint {
    curve: EccCurve,
    x: BigEndianBytes,
    y: BigEndianBytes,
}

impl EccPublicPoint {
    pub fn new(curve: EccCurve, x: BigEndianBytes, y: BigEndianBytes) -> Self {
        Self { curve, x, y }
    }

    pub fn from_be_bytes(curve: EccCurve, x: Vec<u8>, y: Vec<u8>) -> Self {
        Self::new(curve, BigEndianBytes::new(x), BigEndianBytes::new(y))
    }

    pub fn curve(&self) -> EccCurve {
        self.curve
    }

    pub fn x(&self) -> &[u8] {
        self.x.as_bytes()
    }

    pub fn y(&self) -> &[u8] {
        self.y.as_bytes()
    }
}

/// SM2 public point in big-endian byte form.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Sm2PublicPoint {
    x: BigEndianBytes,
    y: BigEndianBytes,
}

impl Sm2PublicPoint {
    pub fn new(x: BigEndianBytes, y: BigEndianBytes) -> Self {
        Self { x, y }
    }

    pub fn from_be_bytes(x: Vec<u8>, y: Vec<u8>) -> Self {
        Self::new(BigEndianBytes::new(x), BigEndianBytes::new(y))
    }

    pub fn x(&self) -> &[u8] {
        self.x.as_bytes()
    }

    pub fn y(&self) -> &[u8] {
        self.y.as_bytes()
    }
}

/// Public key components (algorithm-agnostic).
#[derive(Debug, Clone)]
pub enum PublicKeyComponents {
    /// RSA public key: modulus `n` and public exponent `e`.
    Rsa(RsaPublicComponents),
    /// ECC public key: curve identifier and affine coordinates.
    Ecc(EccPublicPoint),
    /// SM2 public key: affine coordinates (always on the SM2 curve).
    Sm2(Sm2PublicPoint),
}

/// Key pair generation and public key extraction.
pub trait Keypair {
    /// Generate a new key pair using the provided RNG.
    ///
    /// `key_size_bits` is interpreted per algorithm:
    /// - RSA: the bit length of the modulus (e.g. 2048, 4096)
    /// - ECC/SM2: ignored (curve size is fixed)
    fn generate(rng: &mut dyn CryptoRng, key_size_bits: usize) -> Result<Self>
    where
        Self: Sized;

    /// Extract the public key components.
    fn to_public_components(&self) -> Result<PublicKeyComponents>;
}

/// Digital signature generation.
pub trait Signer {
    /// Sign the given message.
    ///
    /// For RSA the message is hashed internally by the signing scheme.
    /// For ECC/SM2 the message is hashed by the curve's default digest.
    fn sign(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<SignatureBytes>;
}

/// Digital signature verification.
pub trait Verifier {
    /// Verify a signature against the given message.
    fn verify(&self, msg: &[u8], signature: &SignatureBytes) -> Result<()>;
}

/// Asymmetric encryption.
pub trait Encryptor {
    /// Encrypt a message.
    fn encrypt(&self, msg: &[u8], rng: &mut dyn CryptoRng) -> Result<CiphertextBytes>;
}

/// Asymmetric decryption.
pub trait Decryptor {
    /// Decrypt a ciphertext.
    fn decrypt(&self, ciphertext: &CiphertextBytes) -> Result<PlaintextBytes>;
}

/// Key agreement (e.g. ECDH).
pub trait KeyAgreement {
    /// Compute a shared secret from a local private key and a peer's public key.
    fn shared_secret(&self, peer_public: &PublicKeyComponents) -> Result<SharedSecretBytes>;
}
