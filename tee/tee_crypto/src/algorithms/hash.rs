// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hash abstraction — SHA-1/224/256/384/512, SM3, MD5.
//!
//! MD5 and SHA-1 are retained for GlobalPlatform TEE API compatibility.
//! Both are considered weak (MD5: broken collisions; SHA-1: deprecated,
//! collision attacks known) and should not be chosen for new security-sensitive
//! use; prefer SHA-256, SHA-384/512, or SM3.

use alloc::vec::Vec;
use core::{fmt, ops::Deref};

use digest::Digest as _;

/// Supported hash algorithm selector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HashAlgorithm {
    /// MD5 — legacy TEE support only; cryptographically broken, not recommended.
    Md5,
    /// SHA-1 — legacy TEE support only; deprecated and weak, not recommended.
    Sha1,
    Sha224,
    Sha256,
    Sha384,
    Sha512,
    Sm3,
}

/// Static metadata for a hash algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HashSpec {
    pub algorithm: HashAlgorithm,
    pub name: &'static str,
    pub output_size: usize,
    pub block_size: usize,
}

/// Hash output tagged with the algorithm that produced it.
#[derive(Clone, Eq, PartialEq)]
pub struct DigestBytes {
    bytes: Vec<u8>,
    algorithm: HashAlgorithm,
}

impl DigestBytes {
    pub fn new(bytes: Vec<u8>, algorithm: HashAlgorithm) -> Self {
        Self { bytes, algorithm }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.bytes
    }
}

impl AsRef<[u8]> for DigestBytes {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for DigestBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl fmt::Debug for DigestBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DigestBytes")
            .field("len", &self.as_bytes().len())
            .field("algorithm", &self.algorithm)
            .finish()
    }
}

impl HashAlgorithm {
    pub const fn spec(self) -> HashSpec {
        match self {
            Self::Md5 => HashSpec {
                algorithm: self,
                name: "MD5",
                output_size: 16,
                block_size: 64,
            },
            Self::Sha1 => HashSpec {
                algorithm: self,
                name: "SHA-1",
                output_size: 20,
                block_size: 64,
            },
            Self::Sha224 => HashSpec {
                algorithm: self,
                name: "SHA-224",
                output_size: 28,
                block_size: 64,
            },
            Self::Sha256 => HashSpec {
                algorithm: self,
                name: "SHA-256",
                output_size: 32,
                block_size: 64,
            },
            Self::Sha384 => HashSpec {
                algorithm: self,
                name: "SHA-384",
                output_size: 48,
                block_size: 128,
            },
            Self::Sha512 => HashSpec {
                algorithm: self,
                name: "SHA-512",
                output_size: 64,
                block_size: 128,
            },
            Self::Sm3 => HashSpec {
                algorithm: self,
                name: "SM3",
                output_size: 32,
                block_size: 64,
            },
        }
    }

    pub const fn output_size(self) -> usize {
        self.spec().output_size
    }

    pub const fn name(self) -> &'static str {
        self.spec().name
    }
}

/// Trait for cryptographic hash functions.
pub trait Digest {
    /// Create a new hash context.
    fn new() -> Self
    where
        Self: Sized;

    /// Feed data into the hash.
    fn update(&mut self, data: &[u8]);

    /// Finalize and return the hash output.
    fn finalize(self) -> DigestBytes;

    /// Return the algorithm selector for this digest implementation.
    fn algorithm() -> HashAlgorithm;

    /// Return the output size in bytes.
    fn output_size() -> usize;

    /// Return the algorithm name.
    fn name() -> &'static str;
}

// Wrapper types around RustCrypto concrete hash implementations.

/// SHA-256 hash.
#[derive(Clone)]
pub struct Sha256 {
    inner: sha2::Sha256,
}

/// SHA-512 hash.
#[derive(Clone)]
pub struct Sha512 {
    inner: sha2::Sha512,
}

/// SM3 hash (Chinese national standard).
#[derive(Clone)]
pub struct Sm3 {
    inner: sm3::Sm3,
}

/// SHA-1 hash.
///
/// Supported for legacy TEE interoperability only; not recommended for new use.
#[derive(Clone)]
pub struct Sha1 {
    inner: sha1::Sha1,
}

/// SHA-224 hash.
#[derive(Clone)]
pub struct Sha224 {
    inner: sha2::Sha224,
}

/// SHA-384 hash.
#[derive(Clone)]
pub struct Sha384 {
    inner: sha2::Sha384,
}

// Macro to implement the Digest trait for RustCrypto-backed wrapper types.
macro_rules! impl_digest {
    ($wrapper:ident, $inner:ty, $algorithm:expr, $size:expr, $label:expr) => {
        impl Digest for $wrapper {
            fn new() -> Self {
                Self {
                    inner: <$inner>::new(),
                }
            }

            fn update(&mut self, data: &[u8]) {
                self.inner.update(data);
            }

            fn finalize(self) -> DigestBytes {
                DigestBytes::new(self.inner.finalize().to_vec(), Self::algorithm())
            }

            fn algorithm() -> HashAlgorithm {
                $algorithm
            }

            fn output_size() -> usize {
                $size
            }

            fn name() -> &'static str {
                $label
            }
        }
    };
}

impl_digest!(Sha256, sha2::Sha256, HashAlgorithm::Sha256, 32, "SHA-256");
impl_digest!(Sha512, sha2::Sha512, HashAlgorithm::Sha512, 64, "SHA-512");
impl_digest!(Sm3, sm3::Sm3, HashAlgorithm::Sm3, 32, "SM3");
impl_digest!(Sha1, sha1::Sha1, HashAlgorithm::Sha1, 20, "SHA-1");
impl_digest!(Sha224, sha2::Sha224, HashAlgorithm::Sha224, 28, "SHA-224");
impl_digest!(Sha384, sha2::Sha384, HashAlgorithm::Sha384, 48, "SHA-384");
