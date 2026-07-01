// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! tee_crypto — Cryptographic abstraction layer backed by RustCrypto.
//!
//! Provides trait-based abstractions for hash, MAC, symmetric/asymmetric
//! cryptography, BigNum, and RNG, replacing mbedtls in the TEE stack.

#![no_std]
#![forbid(unsafe_code)]

#[cfg_attr(all(feature = "pkix", test), macro_use)]
extern crate alloc;

mod algorithms;
pub mod asymmetric;
pub mod bignum;
pub mod bytes;
pub mod error;
pub mod kdf;
pub mod material;
pub mod rng;
pub mod streaming_cipher;
pub mod tee_ops;

#[cfg(feature = "pkix")]
pub(crate) mod pkix_path;

#[cfg(feature = "pkix")]
pub mod pkix;

pub use algorithms::{aead, block_cipher, cipher, ecc, hash, hkdf, mac, md5, rsa, sm2, xts};
pub use error::{CryptoError, Result};
pub use rng::CryptoRng;
pub use tee_ops::ed25519;
