// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CTR mode processing.

use alloc::vec::Vec;

use cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};

use crate::{
    error::{CryptoError, Result},
    streaming_cipher::{algo::StreamingCipherAlgo, context::StreamingCipherCtx},
};

pub(in crate::streaming_cipher) fn process(
    ctx: &mut StreamingCipherCtx,
    data: &[u8],
) -> Result<Vec<u8>> {
    let mut output = data.to_vec();
    match ctx.algo {
        StreamingCipherAlgo::Aes128Ctr => {
            let mut ctr = ctr::Ctr128BE::<aes::Aes128>::new_from_slices(&ctx.key, &ctx.iv)
                .map_err(|_| CryptoError::InvalidKey)?;
            ctr.try_seek(ctx.stream_offset)
                .map_err(|_| CryptoError::InvalidInput)?;
            ctr.apply_keystream(&mut output);
        }
        StreamingCipherAlgo::Aes256Ctr => {
            let mut ctr = ctr::Ctr128BE::<aes::Aes256>::new_from_slices(&ctx.key, &ctx.iv)
                .map_err(|_| CryptoError::InvalidKey)?;
            ctr.try_seek(ctx.stream_offset)
                .map_err(|_| CryptoError::InvalidInput)?;
            ctr.apply_keystream(&mut output);
        }
        StreamingCipherAlgo::Sm4Ctr => {
            let mut ctr = ctr::Ctr128BE::<sm4::Sm4>::new_from_slices(&ctx.key, &ctx.iv)
                .map_err(|_| CryptoError::InvalidKey)?;
            ctr.try_seek(ctx.stream_offset)
                .map_err(|_| CryptoError::InvalidInput)?;
            ctr.apply_keystream(&mut output);
        }
        _ => return Err(CryptoError::InvalidInput),
    }
    ctx.stream_offset = ctx
        .stream_offset
        .checked_add(data.len())
        .ok_or(CryptoError::ArithmeticOverflow)?;
    Ok(output)
}
