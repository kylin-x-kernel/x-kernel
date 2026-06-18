// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block cipher abstraction — AES-128-ECB, AES-256-ECB, SM4-ECB, DES-ECB,
//! DES3-ECB.

use cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use crate::error::{CryptoError, Result};

/// Trait for block cipher operations (single-block ECB mode).
pub trait BlockCipher {
    /// Encrypt a single block in-place.
    fn encrypt(key: &[u8], block: &mut [u8]) -> Result<()>;

    /// Decrypt a single block in-place.
    fn decrypt(key: &[u8], block: &mut [u8]) -> Result<()>;

    /// Return the block size in bytes.
    fn block_size() -> usize;

    /// Return the key size in bytes.
    fn key_size() -> usize;
}

/// Static metadata for a block-cipher wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCipherSpec {
    pub name: &'static str,
    pub block_size: usize,
    pub key_size: usize,
}

macro_rules! define_block_ciphers {
    (
        $(
            $(#[$meta:meta])*
            $wrapper:ident => {
                backend: $cipher:ty,
                name: $name:expr,
                block: $block_len:expr,
                key: $key_len:expr $(,)?
            }
        ),+ $(,)?
    ) => {
        $(
        $(#[$meta])*
        pub struct $wrapper;

        impl $wrapper {
            pub const SPEC: BlockCipherSpec = BlockCipherSpec {
                name: $name,
                block_size: $block_len,
                key_size: $key_len,
            };

            pub const fn spec() -> BlockCipherSpec {
                Self::SPEC
            }
        }

        impl BlockCipher for $wrapper {
            fn encrypt(key: &[u8], block: &mut [u8]) -> Result<()> {
                if block.len() != $block_len {
                    return Err(CryptoError::InvalidLength);
                }
                let cipher = <$cipher>::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let arr: [u8; $block_len] =
                    block.try_into().map_err(|_| CryptoError::InvalidLength)?;
                let mut b = cipher::Block::<$cipher>::from(arr);
                cipher.encrypt_block(&mut b);
                block.copy_from_slice(&b);
                Ok(())
            }

            fn decrypt(key: &[u8], block: &mut [u8]) -> Result<()> {
                if block.len() != $block_len {
                    return Err(CryptoError::InvalidLength);
                }
                let cipher = <$cipher>::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let arr: [u8; $block_len] =
                    block.try_into().map_err(|_| CryptoError::InvalidLength)?;
                let mut b = cipher::Block::<$cipher>::from(arr);
                cipher.decrypt_block(&mut b);
                block.copy_from_slice(&b);
                Ok(())
            }

            fn block_size() -> usize {
                Self::SPEC.block_size
            }

            fn key_size() -> usize {
                Self::SPEC.key_size
            }
        }
        )+
    };
}

define_block_ciphers! {
    /// AES-128 in ECB mode (single block).
    Aes128Ecb => { backend: aes::Aes128, name: "AES-128-ECB", block: 16, key: 16 },
    /// AES-256 in ECB mode (single block).
    Aes256Ecb => { backend: aes::Aes256, name: "AES-256-ECB", block: 16, key: 32 },
    /// SM4 in ECB mode (single block).
    Sm4Ecb => { backend: sm4::Sm4, name: "SM4-ECB", block: 16, key: 16 },
    /// DES in ECB mode (single block).
    DesEcb => { backend: des::Des, name: "DES-ECB", block: 8, key: 8 },
    /// Triple-DES (3DES / TDEA) in ECB mode (single block).
    Des3Ecb => { backend: des::TdesEde3, name: "3DES-ECB", block: 8, key: 24 },
}
