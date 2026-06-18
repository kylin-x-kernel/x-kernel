// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Algorithm-tagged cryptographic material.

use alloc::vec::Vec;
use core::{fmt, ops::Deref};

use crate::{
    asymmetric::EccCurve,
    bytes::{PublicBytes, SecretBytes},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureAlgorithm {
    Ecdsa(EccCurve),
    RsaPkcs1v15,
    RsaPss,
    Sm2Dsa,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureEncoding {
    Raw,
    Der,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SignatureBytes {
    bytes: PublicBytes,
    algorithm: SignatureAlgorithm,
    encoding: SignatureEncoding,
}

impl SignatureBytes {
    pub fn new(bytes: Vec<u8>, algorithm: SignatureAlgorithm, encoding: SignatureEncoding) -> Self {
        Self {
            bytes: PublicBytes::new(bytes),
            algorithm,
            encoding,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    pub fn algorithm(&self) -> SignatureAlgorithm {
        self.algorithm
    }

    pub fn encoding(&self) -> SignatureEncoding {
        self.encoding
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.bytes.into_vec()
    }
}

impl AsRef<[u8]> for SignatureBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for SignatureBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for SignatureBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignatureBytes")
            .field("len", &self.as_bytes().len())
            .field("algorithm", &self.algorithm)
            .field("encoding", &self.encoding)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CiphertextAlgorithm {
    RsaPkcs1v15,
    RsaOaep,
    Sm2Pke,
}

#[derive(Clone, Eq, PartialEq)]
pub struct CiphertextBytes {
    bytes: PublicBytes,
    algorithm: CiphertextAlgorithm,
}

impl CiphertextBytes {
    pub fn new(bytes: Vec<u8>, algorithm: CiphertextAlgorithm) -> Self {
        Self {
            bytes: PublicBytes::new(bytes),
            algorithm,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_bytes()
    }

    pub fn algorithm(&self) -> CiphertextAlgorithm {
        self.algorithm
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.bytes.into_vec()
    }
}

impl AsRef<[u8]> for CiphertextBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for CiphertextBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for CiphertextBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CiphertextBytes")
            .field("len", &self.as_bytes().len())
            .field("algorithm", &self.algorithm)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedSecretAlgorithm {
    Ecdh(EccCurve),
    Sm2Kep,
}

#[derive(Clone, Eq, PartialEq)]
pub struct SharedSecretBytes {
    bytes: SecretBytes,
    algorithm: SharedSecretAlgorithm,
}

impl SharedSecretBytes {
    pub fn new(bytes: Vec<u8>, algorithm: SharedSecretAlgorithm) -> Self {
        Self {
            bytes: SecretBytes::new(bytes),
            algorithm,
        }
    }

    pub fn expose_secret(&self) -> &[u8] {
        self.bytes.expose_secret()
    }

    pub fn algorithm(&self) -> SharedSecretAlgorithm {
        self.algorithm
    }

    pub fn expose_secret_clone(&self) -> Vec<u8> {
        self.bytes.expose_secret_clone()
    }
}

impl fmt::Debug for SharedSecretBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSecretBytes")
            .field("len", &self.expose_secret().len())
            .field("algorithm", &self.algorithm)
            .finish()
    }
}
