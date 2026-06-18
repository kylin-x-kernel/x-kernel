// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! MAC abstraction — HMAC-MD5/SHA1/224/256/384/512/SM3, AES-128/192/256-CMAC,
//! SM4-CMAC, DES3-CMAC.

use alloc::vec::Vec;

use digest::KeyInit;

use crate::error::{CryptoError, Result};

/// Trait for Message Authentication Code computation.
pub trait Mac {
    /// Create a new MAC context from the given key.
    fn new(key: &[u8]) -> Result<Self>
    where
        Self: Sized;

    /// Feed data into the MAC.
    fn update(&mut self, data: &[u8]);

    /// Finalize and return the MAC tag.
    fn finalize(self) -> Vec<u8>;

    /// Return the output tag size in bytes.
    fn output_size() -> usize;
}

/// HMAC-MD5.
#[derive(Clone)]
pub struct HmacMd5 {
    inner: hmac::Hmac<md5::Md5>,
}

/// HMAC-SHA-1.
#[derive(Clone)]
pub struct HmacSha1 {
    inner: hmac::Hmac<sha1::Sha1>,
}

/// HMAC-SHA-224.
#[derive(Clone)]
pub struct HmacSha224 {
    inner: hmac::Hmac<sha2::Sha224>,
}

/// HMAC-SHA-256.
#[derive(Clone)]
pub struct HmacSha256 {
    inner: hmac::Hmac<sha2::Sha256>,
}

/// HMAC-SHA-384.
#[derive(Clone)]
pub struct HmacSha384 {
    inner: hmac::Hmac<sha2::Sha384>,
}

/// HMAC-SHA-512.
#[derive(Clone)]
pub struct HmacSha512 {
    inner: hmac::Hmac<sha2::Sha512>,
}

/// HMAC-SM3.
#[derive(Clone)]
pub struct HmacSm3 {
    inner: hmac::Hmac<sm3::Sm3>,
}

macro_rules! impl_hmac {
    ($wrapper:ident, $hash:ty, $size:expr) => {
        impl Mac for $wrapper {
            fn new(key: &[u8]) -> Result<Self> {
                let inner = hmac::Hmac::<$hash>::new_from_slice(key)
                    .map_err(|_| CryptoError::InvalidKey)?;
                Ok(Self { inner })
            }

            fn update(&mut self, data: &[u8]) {
                digest::Mac::update(&mut self.inner, data);
            }

            fn finalize(self) -> Vec<u8> {
                digest::Mac::finalize(self.inner).into_bytes().to_vec()
            }

            fn output_size() -> usize {
                $size
            }
        }
    };
}

impl_hmac!(HmacMd5, md5::Md5, 16);
impl_hmac!(HmacSha1, sha1::Sha1, 20);
impl_hmac!(HmacSha224, sha2::Sha224, 28);
impl_hmac!(HmacSha256, sha2::Sha256, 32);
impl_hmac!(HmacSha384, sha2::Sha384, 48);
impl_hmac!(HmacSha512, sha2::Sha512, 64);
impl_hmac!(HmacSm3, sm3::Sm3, 32);

/// AES-128-CMAC.
#[derive(Clone)]
pub struct Aes128Cmac {
    inner: cmac::Cmac<aes::Aes128>,
}

/// AES-192-CMAC.
#[derive(Clone)]
pub struct Aes192Cmac {
    inner: cmac::Cmac<aes::Aes192>,
}

/// AES-256-CMAC.
#[derive(Clone)]
pub struct Aes256Cmac {
    inner: cmac::Cmac<aes::Aes256>,
}

/// SM4-CMAC.
#[derive(Clone)]
pub struct Sm4Cmac {
    inner: cmac::Cmac<sm4::Sm4>,
}

macro_rules! impl_cmac {
    ($wrapper:ident, $cipher:ty, $size:expr) => {
        impl Mac for $wrapper {
            fn new(key: &[u8]) -> Result<Self> {
                let inner = cmac::Cmac::<$cipher>::new_from_slice(key)
                    .map_err(|_| CryptoError::InvalidKey)?;
                Ok(Self { inner })
            }

            fn update(&mut self, data: &[u8]) {
                digest::Mac::update(&mut self.inner, data);
            }

            fn finalize(self) -> Vec<u8> {
                digest::Mac::finalize(self.inner).into_bytes().to_vec()
            }

            fn output_size() -> usize {
                $size
            }
        }
    };
}

impl_cmac!(Aes128Cmac, aes::Aes128, 16);
impl_cmac!(Aes192Cmac, aes::Aes192, 16);
impl_cmac!(Aes256Cmac, aes::Aes256, 16);
impl_cmac!(Sm4Cmac, sm4::Sm4, 16);

/// DES3-CMAC (Triple-DES CMAC).
#[derive(Clone)]
pub struct Des3Cmac {
    inner: cmac::Cmac<des::TdesEde3>,
}

impl Mac for Des3Cmac {
    fn new(key: &[u8]) -> Result<Self> {
        let inner = cmac::Cmac::<des::TdesEde3>::new_from_slice(key)
            .map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { inner })
    }

    fn update(&mut self, data: &[u8]) {
        digest::Mac::update(&mut self.inner, data);
    }

    fn finalize(self) -> Vec<u8> {
        digest::Mac::finalize(self.inner).into_bytes().to_vec()
    }

    fn output_size() -> usize {
        8
    }
}
