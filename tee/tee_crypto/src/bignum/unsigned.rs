// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::cmp::Ordering;

use crypto_bigint::BoxedUint;

use crate::error::{CryptoError, Result};

/// An unsigned big number backed by `crypto-bigint`.
#[derive(Debug, Clone)]
pub struct TeeBigNum {
    pub(super) value: BoxedUint,
}

impl TeeBigNum {
    /// Create a new `BigNum` representing zero.
    pub fn new() -> Self {
        TeeBigNum {
            value: BoxedUint::zero(),
        }
    }

    /// Create a `BigNum` from a u32 value.
    pub fn from_u32(value: u32) -> Self {
        TeeBigNum {
            value: BoxedUint::from(value),
        }
    }

    /// Allocate a `BigNum` capable of holding `bits` bits.
    /// The value is initialized to zero.
    pub fn allocate(bits: usize) -> Self {
        TeeBigNum {
            value: BoxedUint::zero_with_precision(bits as u32),
        }
    }

    /// Create a `BigNum` from a big-endian byte representation.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(CryptoError::InvalidInput);
        }
        Ok(TeeBigNum {
            value: BoxedUint::from_be_slice_vartime(bytes),
        })
    }

    /// Serialize the big number as big-endian bytes (minimal representation).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let bytes = self.value.to_be_bytes_trimmed_vartime();
        if bytes.is_empty() {
            Ok(alloc::vec![0])
        } else {
            Ok(bytes.into_vec())
        }
    }

    /// Return the number of significant bits.
    pub fn bit_length(&self) -> usize {
        self.value.bits_vartime() as usize
    }

    /// Return the number of bytes needed to store this value.
    pub fn byte_length(&self) -> usize {
        self.bit_length().div_ceil(8).max(1)
    }

    /// Compare this `BigNum` with another.
    pub fn compare(&self, other: &Self) -> Ordering {
        self.value.cmp(&other.value)
    }

    /// Clear the value (set to zero).
    pub fn clear(&mut self) {
        self.value = BoxedUint::zero();
    }

    /// Get the value as u32 if it fits in 4 bytes.
    pub fn as_u32(&self) -> Result<u32> {
        let bytes = self.to_bytes()?;
        if bytes.len() > 4 {
            return Err(CryptoError::ArithmeticOverflow);
        }
        let mut buf = [0u8; 4];
        let offset = 4 - bytes.len();
        buf[offset..].copy_from_slice(&bytes);
        Ok(u32::from_be_bytes(buf))
    }

    /// Return true when the value is zero.
    pub fn is_zero(&self) -> bool {
        self.value.is_zero().into()
    }

    pub(super) fn from_boxed(value: BoxedUint) -> Self {
        Self { value }
    }
}

impl Default for TeeBigNum {
    fn default() -> Self {
        Self::new()
    }
}

/// Trait for big number operations — delegates to TeeBigNum inherent methods.
pub trait BigNum {
    fn from_bytes(bytes: &[u8]) -> Result<Self>
    where
        Self: Sized;
    fn to_bytes(&self) -> Result<Vec<u8>>;
    fn bit_length(&self) -> usize;
    fn compare(&self, other: &Self) -> Ordering;
}

impl BigNum for TeeBigNum {
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        TeeBigNum::from_bytes(bytes)
    }

    fn to_bytes(&self) -> Result<Vec<u8>> {
        TeeBigNum::to_bytes(self)
    }

    fn bit_length(&self) -> usize {
        TeeBigNum::bit_length(self)
    }

    fn compare(&self, other: &Self) -> Ordering {
        TeeBigNum::compare(self, other)
    }
}
