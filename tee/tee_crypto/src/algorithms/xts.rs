// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! XTS cipher abstraction — AES-128-XTS, AES-256-XTS, SM4-XTS.

use crate::{
    block_cipher::BlockCipher,
    error::{CryptoError, Result},
};

/// Trait for XTS-mode disk encryption ciphers.
///
/// XTS uses a double-key scheme: the `key` parameter is the concatenation
/// of the data encryption key and the tweak encryption key.
pub trait XtsCipher {
    /// Encrypt `data` in-place using XTS mode.
    fn encrypt(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()>;

    /// Decrypt `data` in-place using XTS mode.
    fn decrypt(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()>;

    /// Total key size in bytes (data key + tweak key).
    fn key_size() -> usize;
}

// GF(2^128) multiplication by alpha (x)

/// Multiply a 16-byte value by x in GF(2^128) using the reduction polynomial
/// x^128 + x^7 + x^2 + x + 1 (0x87 in the constant lookup).
#[allow(clippy::needless_range_loop)]
fn gf128_mul(x: &mut [u8; 16]) {
    let mut feedback: u8 = 0;
    for i in 0..16 {
        let tmp = x[i];
        x[i] = (tmp << 1) | feedback;
        feedback = tmp >> 7;
    }
    if feedback != 0 {
        x[0] ^= 0x87;
    }
}

/// Perform XTS encryption using the given block cipher.
///
/// `key` is data_key || tweak_key, each `B::key_size()` bytes.
/// `tweak` is the 16-byte sector tweak (e.g., sector number as little-endian).
/// `data` is encrypted in-place. Its length must be at least 16 bytes (one block).
fn xts_encrypt<B: BlockCipher>(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()> {
    let half = B::key_size();
    if key.len() != half * 2 {
        return Err(CryptoError::InvalidKey);
    }
    if tweak.len() != 16 {
        return Err(CryptoError::InvalidLength);
    }
    if data.len() < 16 {
        return Err(CryptoError::InvalidInput);
    }

    let data_key = &key[..half];
    let tweak_key = &key[half..];

    // Encrypt the tweak with the tweak key to get the initial XTS tweak value
    let mut xts_tweak: [u8; 16] = tweak.try_into().map_err(|_| CryptoError::InvalidLength)?;
    B::encrypt(tweak_key, &mut xts_tweak)?;

    let full_blocks = data.len() / 16;
    let remainder = data.len() % 16;

    for i in 0..full_blocks {
        let start = i * 16;
        let block = &mut data[start..start + 16];

        // XOR block with tweak
        for j in 0..16 {
            block[j] ^= xts_tweak[j];
        }

        // Encrypt with data key
        B::encrypt(data_key, block)?;

        // XOR again with tweak
        for j in 0..16 {
            block[j] ^= xts_tweak[j];
        }

        // Advance tweak via GF(2^128) multiplication
        gf128_mul(&mut xts_tweak);
    }

    // Handle partial last block (ciphertext stealing)
    if remainder > 0 {
        // The last full block becomes the "stolen" block.
        // We swap the partial tail with the end of the last full block,
        // then encrypt that combined block.
        let last_full = full_blocks - 1;
        let last_full_start = last_full * 16;

        // Copy last full block to a temporary buffer
        let mut cc: [u8; 16] = [0u8; 16];
        cc.copy_from_slice(&data[last_full_start..last_full_start + 16]);

        // Copy the partial tail bytes over the last full block
        let tail_start = full_blocks * 16;
        let tmp = data[tail_start..tail_start + remainder].to_vec();
        data[last_full_start..last_full_start + remainder].copy_from_slice(&tmp);
        // Pad the rest with the corresponding bytes from cc
        data[last_full_start + remainder..last_full_start + 16].copy_from_slice(&cc[remainder..16]);

        // Re-derive the tweak for this position (already at full_blocks)
        // We need to recompute: we already advanced past the last full block,
        // but we need the tweak for position `full_blocks` which is already
        // in xts_tweak after the loop. However, we used position `last_full`
        // for the combined block, not position `full_blocks`. Let's fix:
        // We need to undo one step and use the tweak at position full_blocks.
        // Actually xts_tweak is already at the right position (after full_blocks
        // iterations), which corresponds to the combined block.

        // XOR with tweak
        let combined_start = last_full_start;
        for j in 0..16 {
            data[combined_start + j] ^= xts_tweak[j];
        }
        B::encrypt(data_key, &mut data[combined_start..combined_start + 16])?;
        for j in 0..16 {
            data[combined_start + j] ^= xts_tweak[j];
        }

        // Now put the stolen ciphertext bytes into the tail
        data[tail_start..tail_start + remainder].copy_from_slice(&cc[..remainder]);
    }

    Ok(())
}

/// Perform XTS decryption using the given block cipher.
fn xts_decrypt<B: BlockCipher>(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()> {
    let half = B::key_size();
    if key.len() != half * 2 {
        return Err(CryptoError::InvalidKey);
    }
    if tweak.len() != 16 {
        return Err(CryptoError::InvalidLength);
    }
    if data.len() < 16 {
        return Err(CryptoError::InvalidInput);
    }

    let data_key = &key[..half];
    let tweak_key = &key[half..];

    let mut xts_tweak: [u8; 16] = tweak.try_into().map_err(|_| CryptoError::InvalidLength)?;
    B::encrypt(tweak_key, &mut xts_tweak)?;

    let full_blocks = data.len() / 16;
    let remainder = data.len() % 16;

    if remainder == 0 {
        // Simple case: all full blocks
        for i in 0..full_blocks {
            let start = i * 16;
            let block = &mut data[start..start + 16];

            for j in 0..16 {
                block[j] ^= xts_tweak[j];
            }
            B::decrypt(data_key, block)?;
            for j in 0..16 {
                block[j] ^= xts_tweak[j];
            }

            gf128_mul(&mut xts_tweak);
        }
    } else {
        // Ciphertext stealing for partial last block.
        // First, compute the tweak values we need.
        // We need the tweak at position (full_blocks - 1) and at position full_blocks.

        // Advance tweak to position (full_blocks - 1)
        let mut tweak_nm1: [u8; 16] = xts_tweak;
        for _ in 0..full_blocks - 1 {
            gf128_mul(&mut tweak_nm1);
        }
        let mut tweak_n: [u8; 16] = tweak_nm1;
        gf128_mul(&mut tweak_n);

        // Decrypt the last full block using tweak_n (it was encrypted with tweak_n
        // during ciphertext stealing). This gives us the "stolen" plaintext bytes.
        let last_full = full_blocks - 1;
        let last_full_start = last_full * 16;

        let mut pp: [u8; 16] = [0u8; 16];
        pp.copy_from_slice(&data[last_full_start..last_full_start + 16]);
        for j in 0..16 {
            pp[j] ^= tweak_n[j];
        }
        // Decrypt in-place on the array
        {
            let mut block_arr = pp;
            B::decrypt(data_key, &mut block_arr)?;
            pp = block_arr;
        }
        for j in 0..16 {
            pp[j] ^= tweak_n[j];
        }

        // Now pp contains the plaintext of the combined block.
        // The first `remainder` bytes of the tail are the stolen ciphertext.
        // Swap them: put the stolen bytes into the last full block position,
        // and put the tail of pp into the partial block position.
        let tail_start = full_blocks * 16;

        // Build the combined ciphertext block
        let mut cc: [u8; 16] = [0u8; 16];
        cc[..remainder].copy_from_slice(&data[tail_start..tail_start + remainder]);
        cc[remainder..16].copy_from_slice(&pp[remainder..16]);

        // Decrypt the combined block with tweak_nm1
        for j in 0..16 {
            cc[j] ^= tweak_nm1[j];
        }
        B::decrypt(data_key, &mut cc)?;
        for j in 0..16 {
            cc[j] ^= tweak_nm1[j];
        }

        // Write back results
        // The last full block position gets the combined plaintext
        data[last_full_start..last_full_start + 16].copy_from_slice(&cc);
        // The partial tail gets the first `remainder` bytes of pp
        data[tail_start..tail_start + remainder].copy_from_slice(&pp[..remainder]);

        // Decrypt all preceding full blocks
        // We need to redo from the start since we modified data in place
        // Actually, we haven't touched the preceding blocks yet.
        // Re-derive tweak from the beginning.
        let mut tw: [u8; 16] = tweak.try_into().map_err(|_| CryptoError::InvalidLength)?;
        B::encrypt(tweak_key, &mut tw)?;

        for i in 0..last_full {
            let start = i * 16;
            let block = &mut data[start..start + 16];

            for j in 0..16 {
                block[j] ^= tw[j];
            }
            B::decrypt(data_key, block)?;
            for j in 0..16 {
                block[j] ^= tw[j];
            }

            gf128_mul(&mut tw);
        }
    }

    Ok(())
}

/// AES-128-XTS (key = 32 bytes: 16 data + 16 tweak).
pub struct Aes128Xts;

/// AES-256-XTS (key = 64 bytes: 32 data + 32 tweak).
pub struct Aes256Xts;

/// SM4-XTS (key = 32 bytes: 16 data + 16 tweak).
pub struct Sm4Xts;

macro_rules! impl_xts {
    ($wrapper:ident, $block:ty, $single_key:expr) => {
        impl XtsCipher for $wrapper {
            fn encrypt(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()> {
                xts_encrypt::<$block>(key, tweak, data)
            }

            fn decrypt(key: &[u8], tweak: &[u8], data: &mut [u8]) -> Result<()> {
                xts_decrypt::<$block>(key, tweak, data)
            }

            fn key_size() -> usize {
                $single_key * 2
            }
        }
    };
}

impl_xts!(Aes128Xts, crate::block_cipher::Aes128Ecb, 16);
impl_xts!(Aes256Xts, crate::block_cipher::Aes256Ecb, 32);
impl_xts!(Sm4Xts, crate::block_cipher::Sm4Ecb, 16);
