// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GCM/CCM dispatch for the streaming cipher context.

use alloc::vec::Vec;

use cipher::{
    BlockCipherEncrypt, BlockSizeUser, InnerIvInit, KeyInit, StreamCipherCore, consts::U16,
};
use ghash::GHash;

use crate::{
    aead::{
        Aead, Aes128GcmAead, Aes192GcmAead, Aes256GcmAead, Sm4GcmAead, ccm_decrypt, ccm_encrypt,
        compute_gcm_j0, compute_gcm_tag, gcm_ctr_from_key,
    },
    error::{CryptoError, Result},
    streaming_cipher::{
        algo::{Direction, StreamingCipherAlgo},
        context::StreamingCipherCtx,
    },
};

pub(in crate::streaming_cipher) fn update(
    ctx: &mut StreamingCipherCtx,
    input: &[u8],
) -> Result<Vec<u8>> {
    if ctx.algo.is_gcm() {
        let output = gcm_ctr_chunk(ctx, input)?;
        if ctx.direction.is_encrypting() {
            ctx.ciphertext.extend_from_slice(&output);
        } else {
            ctx.ciphertext.extend_from_slice(input);
            ctx.plaintext.extend_from_slice(&output);
        }
        ctx.returned_len += output.len();
        Ok(output)
    } else {
        ctx.buffer.extend_from_slice(input);
        Ok(Vec::new())
    }
}

/// One-shot AEAD encrypt/decrypt using buffered data.
pub(in crate::streaming_cipher) fn one_shot(ctx: StreamingCipherCtx) -> Result<Vec<u8>> {
    let nonce = &ctx.iv;
    match ctx.algo {
        StreamingCipherAlgo::Aes128Gcm => {
            if ctx.direction.is_encrypting() {
                Aes128GcmAead::encrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            } else {
                Aes128GcmAead::decrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            }
        }
        StreamingCipherAlgo::Aes192Gcm => {
            if ctx.direction.is_encrypting() {
                Aes192GcmAead::encrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            } else {
                Aes192GcmAead::decrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            }
        }
        StreamingCipherAlgo::Aes256Gcm => {
            if ctx.direction.is_encrypting() {
                Aes256GcmAead::encrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            } else {
                Aes256GcmAead::decrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            }
        }
        StreamingCipherAlgo::Sm4Gcm => {
            if ctx.direction.is_encrypting() {
                Sm4GcmAead::encrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            } else {
                Sm4GcmAead::decrypt(&ctx.key, nonce, &ctx.aad, &ctx.buffer)
            }
        }
        StreamingCipherAlgo::Aes128Ccm => ccm_with_direction::<aes::Aes128>(&ctx),
        StreamingCipherAlgo::Aes256Ccm => ccm_with_direction::<aes::Aes256>(&ctx),
        _ => Err(CryptoError::UnsupportedAlgorithm),
    }
}

fn ccm_with_direction<C>(ctx: &StreamingCipherCtx) -> Result<Vec<u8>>
where
    C: KeyInit + BlockCipherEncrypt + BlockSizeUser<BlockSize = U16>,
{
    match ctx.direction {
        Direction::Encrypt => {
            ccm_encrypt::<C>(&ctx.key, &ctx.iv, &ctx.aad, &ctx.buffer, ctx.tag_len)
        }
        Direction::Decrypt => {
            ccm_decrypt::<C>(&ctx.key, &ctx.iv, &ctx.aad, &ctx.buffer, ctx.tag_len)
        }
    }
}

/// CTR-encrypt/decrypt a chunk at the current keystream position (GCM).
fn gcm_ctr_chunk(ctx: &StreamingCipherCtx, input: &[u8]) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }
    let offset = ctx.ciphertext.len();
    let nonce = &ctx.iv;

    match ctx.algo {
        StreamingCipherAlgo::Aes128Gcm => {
            let ctr = gcm_ctr_from_key::<aes::Aes128>(&ctx.key, nonce, offset)?;
            gcm_apply_keystream(ctr, offset, input)
        }
        StreamingCipherAlgo::Aes192Gcm => {
            let ctr = gcm_ctr_from_key::<aes::Aes192>(&ctx.key, nonce, offset)?;
            gcm_apply_keystream(ctr, offset, input)
        }
        StreamingCipherAlgo::Aes256Gcm => {
            let ctr = gcm_ctr_from_key::<aes::Aes256>(&ctx.key, nonce, offset)?;
            gcm_apply_keystream(ctr, offset, input)
        }
        StreamingCipherAlgo::Sm4Gcm => {
            let ctr = gcm_ctr_from_key::<sm4::Sm4>(&ctx.key, nonce, offset)?;
            gcm_apply_keystream(ctr, offset, input)
        }
        _ => Err(CryptoError::UnsupportedAlgorithm),
    }
}

fn gcm_apply_keystream<C>(
    mut ctr: ctr::CtrCore<C, ctr::flavors::Ctr32BE>,
    offset: usize,
    input: &[u8],
) -> Result<Vec<u8>>
where
    C: KeyInit + BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    let mut output = input.to_vec();
    let partial_offset = offset % 16;
    if partial_offset == 0 {
        ctr.apply_keystream_partial((&mut output[..]).into());
        return Ok(output);
    }

    let mut block = cipher::Array::<u8, U16>::default();
    ctr.write_keystream_block(&mut block);
    let head_len = output.len().min(16 - partial_offset);
    for (out, key) in output[..head_len]
        .iter_mut()
        .zip(block[partial_offset..partial_offset + head_len].iter())
    {
        *out ^= key;
    }
    if head_len < output.len() {
        ctr.apply_keystream_partial((&mut output[head_len..]).into());
    }
    Ok(output)
}

/// Build (GHash, J0, tag_mask) from key and nonce for any GCM variant.
#[allow(clippy::type_complexity)]
fn gcm_auth_primitives<C>(
    ctx: &StreamingCipherCtx,
) -> Result<(GHash, cipher::Array<u8, U16>, cipher::Array<u8, U16>)>
where
    C: KeyInit + BlockCipherEncrypt + cipher::BlockSizeUser<BlockSize = U16>,
{
    let cipher = C::new_from_slice(&ctx.key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ghash_key = ghash::Key::default();
    cipher.encrypt_block(&mut ghash_key);
    let ghash = GHash::new(&ghash_key);
    let j0 = compute_gcm_j0(&ghash, &ctx.iv);
    let cipher = C::new_from_slice(&ctx.key).map_err(|_| CryptoError::InvalidKey)?;
    let mut ctr = ctr::CtrCore::<C, ctr::flavors::Ctr32BE>::inner_iv_init(cipher, &j0);
    let mut mask = cipher::Array::<u8, U16>::default();
    ctr.write_keystream_block(&mut mask);
    Ok((ghash, j0, mask))
}

pub(in crate::streaming_cipher) fn compute_tag(ctx: &StreamingCipherCtx) -> Result<[u8; 16]> {
    let (ghash, _j0, tag_mask) = match ctx.algo {
        StreamingCipherAlgo::Aes128Gcm => gcm_auth_primitives::<aes::Aes128>(ctx)?,
        StreamingCipherAlgo::Aes192Gcm => gcm_auth_primitives::<aes::Aes192>(ctx)?,
        StreamingCipherAlgo::Aes256Gcm => gcm_auth_primitives::<aes::Aes256>(ctx)?,
        StreamingCipherAlgo::Sm4Gcm => gcm_auth_primitives::<sm4::Sm4>(ctx)?,
        _ => return Err(CryptoError::UnsupportedAlgorithm),
    };

    Ok(compute_gcm_tag(&ghash, &ctx.aad, &ctx.ciphertext, tag_mask))
}
