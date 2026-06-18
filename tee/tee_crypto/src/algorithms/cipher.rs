// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Streaming symmetric cipher — CBC and CTR modes.
//!
//! Provides one-shot encrypt/decrypt operations with PKCS7 padding for CBC
//! and in-place CTR. Uses the same RustCrypto backend as block_cipher.

use alloc::vec::Vec;

use cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit, KeyIvInit, StreamCipher};
use ctr::Ctr128BE;

use crate::{
    error::{CryptoError, Result},
    streaming_cipher::padding::{pkcs7_pad, pkcs7_unpad},
};

/// Encrypt plaintext using CBC mode with PKCS7 padding.
pub fn cbc_encrypt<C>(key: &[u8], iv: &[u8], plaintext: &[u8], block_size: usize) -> Result<Vec<u8>>
where
    C: BlockCipherEncrypt + KeyInit,
{
    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let bs = block_size;
    if iv.len() != bs {
        return Err(CryptoError::InvalidLength);
    }

    let mut input = Vec::with_capacity(plaintext.len() + bs);
    input.extend_from_slice(plaintext);
    pkcs7_pad(&mut input, bs)?;

    let mut output = alloc::vec![0u8; input.len()];
    let mut prev = iv;

    for (i, chunk) in input.chunks(bs).enumerate() {
        let mut block: Vec<u8> = chunk.to_vec();
        for (a, b) in block.iter_mut().zip(prev.iter()) {
            *a ^= b;
        }
        let mut cipher_block =
            cipher::Block::<C>::try_from(&block[..bs]).map_err(|_| CryptoError::InvalidLength)?;
        cipher.encrypt_block(&mut cipher_block);
        output[i * bs..(i + 1) * bs].copy_from_slice(&cipher_block);
        prev = &output[i * bs..(i + 1) * bs];
    }

    Ok(output)
}

/// Decrypt ciphertext using CBC mode with PKCS7 unpadding.
pub fn cbc_decrypt<C>(
    key: &[u8],
    iv: &[u8],
    ciphertext: &[u8],
    block_size: usize,
) -> Result<Vec<u8>>
where
    C: BlockCipherDecrypt + KeyInit,
{
    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let bs = block_size;
    if iv.len() != bs {
        return Err(CryptoError::InvalidLength);
    }
    if ciphertext.is_empty() || !ciphertext.len().is_multiple_of(bs) {
        return Err(CryptoError::InvalidLength);
    }

    let mut output = alloc::vec![0u8; ciphertext.len()];
    let mut prev = iv;

    for (i, chunk) in ciphertext.chunks(bs).enumerate() {
        let mut cipher_block =
            cipher::Block::<C>::try_from(chunk).map_err(|_| CryptoError::InvalidLength)?;
        cipher.decrypt_block(&mut cipher_block);
        let plain_block = &mut output[i * bs..(i + 1) * bs];
        plain_block.copy_from_slice(&cipher_block);
        for (a, b) in plain_block.iter_mut().zip(prev.iter()) {
            *a ^= b;
        }
        prev = chunk;
    }

    pkcs7_unpad(&output, bs)
}

pub fn aes128_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
    cbc_encrypt::<aes::Aes128>(key, iv, pt, 16)
}
pub fn aes128_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cbc_decrypt::<aes::Aes128>(key, iv, ct, 16)
}
pub fn aes256_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
    cbc_encrypt::<aes::Aes256>(key, iv, pt, 16)
}
pub fn aes256_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cbc_decrypt::<aes::Aes256>(key, iv, ct, 16)
}
pub fn sm4_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
    cbc_encrypt::<sm4::Sm4>(key, iv, pt, 16)
}
pub fn sm4_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cbc_decrypt::<sm4::Sm4>(key, iv, ct, 16)
}
pub fn des3_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
    cbc_encrypt::<des::TdesEde3>(key, iv, pt, 8)
}
pub fn des3_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cbc_decrypt::<des::TdesEde3>(key, iv, ct, 8)
}
pub fn des_cbc_encrypt(key: &[u8], iv: &[u8], pt: &[u8]) -> Result<Vec<u8>> {
    cbc_encrypt::<des::Des>(key, iv, pt, 8)
}
pub fn des_cbc_decrypt(key: &[u8], iv: &[u8], ct: &[u8]) -> Result<Vec<u8>> {
    cbc_decrypt::<des::Des>(key, iv, ct, 8)
}

/// AES-128-CTR (in-place)
pub fn aes128_ctr(key: &[u8], iv: &[u8], data: &mut [u8]) -> Result<()> {
    let mut ctr =
        Ctr128BE::<aes::Aes128>::new_from_slices(key, iv).map_err(|_| CryptoError::InvalidKey)?;
    ctr.apply_keystream(data);
    Ok(())
}

/// AES-256-CTR (in-place)
pub fn aes256_ctr(key: &[u8], iv: &[u8], data: &mut [u8]) -> Result<()> {
    let mut ctr =
        Ctr128BE::<aes::Aes256>::new_from_slices(key, iv).map_err(|_| CryptoError::InvalidKey)?;
    ctr.apply_keystream(data);
    Ok(())
}

/// SM4-CTR (in-place)
pub fn sm4_ctr(key: &[u8], iv: &[u8], data: &mut [u8]) -> Result<()> {
    let mut ctr =
        Ctr128BE::<sm4::Sm4>::new_from_slices(key, iv).map_err(|_| CryptoError::InvalidKey)?;
    ctr.apply_keystream(data);
    Ok(())
}
