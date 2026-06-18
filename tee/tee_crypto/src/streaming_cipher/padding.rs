// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Padding helpers shared by streaming cipher modes.

use alloc::vec::Vec;

use crate::error::{CryptoError, Result};

pub(crate) fn pkcs7_pad(data: &mut Vec<u8>, block_size: usize) -> Result<()> {
    if block_size == 0 || block_size > u8::MAX as usize {
        return Err(CryptoError::InvalidLength);
    }
    let pad_len = block_size - (data.len() % block_size);
    data.extend(core::iter::repeat_n(pad_len as u8, pad_len));
    Ok(())
}

pub(crate) fn pkcs7_unpad(data: &[u8], block_size: usize) -> Result<Vec<u8>> {
    if block_size == 0 || data.is_empty() || !data.len().is_multiple_of(block_size) {
        return Err(CryptoError::InvalidInput);
    }
    let pad_byte = *data.last().ok_or(CryptoError::InvalidInput)? as usize;
    if pad_byte == 0 || pad_byte > block_size {
        return Err(CryptoError::InvalidInput);
    }
    if data[data.len() - pad_byte..]
        .iter()
        .any(|&b| b as usize != pad_byte)
    {
        return Err(CryptoError::InvalidInput);
    }
    Ok(data[..data.len() - pad_byte].to_vec())
}
