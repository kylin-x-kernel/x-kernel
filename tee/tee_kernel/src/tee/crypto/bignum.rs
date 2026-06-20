// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 Kylin Soft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cmp::Ordering;

use tee_crypto::bignum::TeeBigNum;
use tee_raw_sys::*;

use crate::tee::{TeeResult, config::CFG_CORE_BIGNUM_MAX_BITS};

/// BigNum wrapper — delegates to tee_crypto::bignum::TeeBigNum.
#[derive(Debug, Clone)]
pub struct BigNum(pub TeeBigNum);

impl BigNum {
    /// Number of bits required to store this value.
    pub fn bit_length(&self) -> usize {
        self.0.bit_length()
    }

    /// Serialize to big-endian bytes.
    pub fn to_bytes(&self) -> TeeResult<alloc::vec::Vec<u8>> {
        self.0.to_bytes().map_err(|_| TEE_ERROR_GENERIC)
    }

    /// Deserialize from big-endian bytes.
    pub fn from_bytes(bytes: &[u8]) -> TeeResult<Self> {
        Ok(BigNum(
            TeeBigNum::from_bytes(bytes).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?,
        ))
    }

    /// Clear the value (set to zero).
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

impl core::default::Default for BigNum {
    fn default() -> Self {
        BigNum(TeeBigNum::new())
    }
}

impl PartialEq for BigNum {
    fn eq(&self, other: &Self) -> bool {
        self.0.compare(&other.0) == Ordering::Equal
    }
}

impl Eq for BigNum {}

#[cfg(unittest)]
impl BigNum {
    pub fn new(value: u32) -> TeeResult<Self> {
        Ok(BigNum(TeeBigNum::from_u32(value)))
    }

    pub fn byte_length(&self) -> usize {
        self.0.byte_length()
    }

    pub fn as_u32(&self) -> TeeResult<u32> {
        self.0.as_u32().map_err(|_| TEE_ERROR_GENERIC)
    }
}

/// Get number of bytes required to store the big number.
pub fn crypto_bignum_num_bytes(a: &BigNum) -> TeeResult<usize> {
    Ok(a.0.byte_length())
}

/// Get number of bits required to store the big number.
pub fn crypto_bignum_num_bits(a: &BigNum) -> TeeResult<usize> {
    Ok(a.0.bit_length())
}

/// Convert big number to binary representation.
pub fn crypto_bignum_bn2bin(from: &BigNum, to: &mut [u8]) -> TeeResult {
    let a = from.0.to_bytes().map_err(|_| TEE_ERROR_GENERIC)?;
    to[..a.len()].copy_from_slice(&a);
    Ok(())
}

/// Convert binary representation to big number.
pub fn crypto_bignum_bin2bn(from: &[u8], to: &mut BigNum) -> TeeResult {
    to.0 = TeeBigNum::from_bytes(from).map_err(|_| TEE_ERROR_BAD_PARAMETERS)?;
    Ok(())
}

/// Copy big number from `from` to `to`.
pub fn crypto_bignum_copy(to: &mut BigNum, from: &BigNum) {
    to.0 = from.0.clone();
}

/// Allocate a big number with specified size in bits.
pub fn crypto_bignum_allocate(size_bits: usize) -> TeeResult<BigNum> {
    let mut size_bits = size_bits;
    if size_bits > CFG_CORE_BIGNUM_MAX_BITS {
        size_bits = CFG_CORE_BIGNUM_MAX_BITS;
    }
    Ok(BigNum(TeeBigNum::allocate(size_bits)))
}
