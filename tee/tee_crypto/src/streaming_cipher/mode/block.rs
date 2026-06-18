// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ECB/CBC block-mode processing.

use alloc::vec::Vec;

use cipher::{Block, BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use crate::{
    block_cipher::{BlockCipher, Sm4Ecb},
    error::{CryptoError, Result},
    streaming_cipher::{
        algo::{Direction, StreamingCipherAlgo},
        context::StreamingCipherCtx,
    },
};

pub(in crate::streaming_cipher) fn process(
    ctx: &mut StreamingCipherCtx,
    data: &[u8],
) -> Result<Vec<u8>> {
    process_raw(&ctx.algo, &ctx.key, &mut ctx.iv, data, ctx.direction)
}

fn process_raw(
    algo: &StreamingCipherAlgo,
    key: &[u8],
    iv: &mut [u8],
    data: &[u8],
    direction: Direction,
) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let block_size = algo.block_size();
    match algo {
        StreamingCipherAlgo::Aes128Cbc | StreamingCipherAlgo::Aes256Cbc => {
            cbc_process_aes(key, iv, data, block_size, direction)
        }
        StreamingCipherAlgo::Sm4Cbc => {
            cbc_process_single::<sm4::Sm4>(key, iv, data, block_size, direction)
        }
        StreamingCipherAlgo::Des3Cbc => {
            cbc_process_single::<des::TdesEde3>(key, iv, data, block_size, direction)
        }
        StreamingCipherAlgo::DesCbc => {
            cbc_process_single::<des::Des>(key, iv, data, block_size, direction)
        }
        StreamingCipherAlgo::Aes128Ecb => {
            ecb_process::<aes::Aes128>(key, data, block_size, direction)
        }
        StreamingCipherAlgo::Aes256Ecb => {
            ecb_process::<aes::Aes256>(key, data, block_size, direction)
        }
        StreamingCipherAlgo::Sm4Ecb => sm4_ecb_process(key, data, block_size, direction),
        StreamingCipherAlgo::Des3Ecb => {
            ecb_process::<des::TdesEde3>(key, data, block_size, direction)
        }
        StreamingCipherAlgo::DesEcb => ecb_process::<des::Des>(key, data, block_size, direction),
        _ => Err(CryptoError::InvalidInput),
    }
}

fn cbc_process_aes(
    key: &[u8],
    iv: &mut [u8],
    data: &[u8],
    block_size: usize,
    direction: Direction,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks(block_size) {
        if chunk.len() != block_size {
            output.extend_from_slice(chunk);
            continue;
        }
        if direction.is_encrypting() {
            let mut block = chunk.to_vec();
            for (a, b) in block.iter_mut().zip(iv.iter()) {
                *a ^= b;
            }
            if key.len() == 32 {
                let cipher =
                    aes::Aes256::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let mut cipher_block = Block::<aes::Aes256>::try_from(&block[..block_size])
                    .map_err(|_| CryptoError::InvalidInput)?;
                cipher.encrypt_block(&mut cipher_block);
                output.extend_from_slice(&cipher_block);
                iv.copy_from_slice(&cipher_block);
            } else {
                let cipher =
                    aes::Aes128::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let mut cipher_block = Block::<aes::Aes128>::try_from(&block[..block_size])
                    .map_err(|_| CryptoError::InvalidInput)?;
                cipher.encrypt_block(&mut cipher_block);
                output.extend_from_slice(&cipher_block);
                iv.copy_from_slice(&cipher_block);
            }
        } else {
            if key.len() == 32 {
                let cipher =
                    aes::Aes256::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let mut plain_block =
                    Block::<aes::Aes256>::try_from(chunk).map_err(|_| CryptoError::InvalidInput)?;
                cipher.decrypt_block(&mut plain_block);
                for (a, b) in plain_block.iter_mut().zip(iv.iter()) {
                    *a ^= b;
                }
                output.extend_from_slice(&plain_block);
            } else {
                let cipher =
                    aes::Aes128::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
                let mut plain_block =
                    Block::<aes::Aes128>::try_from(chunk).map_err(|_| CryptoError::InvalidInput)?;
                cipher.decrypt_block(&mut plain_block);
                for (a, b) in plain_block.iter_mut().zip(iv.iter()) {
                    *a ^= b;
                }
                output.extend_from_slice(&plain_block);
            }
            iv.copy_from_slice(chunk);
        }
    }
    Ok(output)
}

fn cbc_process_single<C>(
    key: &[u8],
    iv: &mut [u8],
    data: &[u8],
    block_size: usize,
    direction: Direction,
) -> Result<Vec<u8>>
where
    C: BlockCipherEncrypt + BlockCipherDecrypt + KeyInit,
{
    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks(block_size) {
        if chunk.len() != block_size {
            output.extend_from_slice(chunk);
            continue;
        }
        if direction.is_encrypting() {
            let mut block = chunk.to_vec();
            for (a, b) in block.iter_mut().zip(iv.iter()) {
                *a ^= b;
            }
            let mut cipher_block = Block::<C>::try_from(&block[..block_size])
                .map_err(|_| CryptoError::InvalidInput)?;
            cipher.encrypt_block(&mut cipher_block);
            output.extend_from_slice(&cipher_block);
            iv.copy_from_slice(&cipher_block);
        } else {
            let mut plain_block =
                Block::<C>::try_from(chunk).map_err(|_| CryptoError::InvalidInput)?;
            cipher.decrypt_block(&mut plain_block);
            for (a, b) in plain_block.iter_mut().zip(iv.iter()) {
                *a ^= b;
            }
            output.extend_from_slice(&plain_block);
            iv.copy_from_slice(chunk);
        }
    }
    Ok(output)
}

fn ecb_process<C>(
    key: &[u8],
    data: &[u8],
    block_size: usize,
    direction: Direction,
) -> Result<Vec<u8>>
where
    C: BlockCipherEncrypt + BlockCipherDecrypt + KeyInit,
{
    let cipher = C::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks(block_size) {
        if chunk.len() != block_size {
            output.extend_from_slice(chunk);
            continue;
        }
        let mut block = Block::<C>::try_from(chunk).map_err(|_| CryptoError::InvalidInput)?;
        if direction.is_encrypting() {
            cipher.encrypt_block(&mut block);
        } else {
            cipher.decrypt_block(&mut block);
        }
        output.extend_from_slice(&block);
    }
    Ok(output)
}

fn sm4_ecb_process(
    key: &[u8],
    data: &[u8],
    block_size: usize,
    direction: Direction,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(data.len());
    for chunk in data.chunks(block_size) {
        if chunk.len() != block_size {
            output.extend_from_slice(chunk);
            continue;
        }
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        if direction.is_encrypting() {
            Sm4Ecb::encrypt(key, &mut block)?;
        } else {
            Sm4Ecb::decrypt(key, &mut block)?;
        }
        output.extend_from_slice(&block);
    }
    Ok(output)
}
