// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;
use core::cmp::Ordering;

use crypto_bigint::{
    BitOps, BoxedUint, ConcatenatingMul, ConcatenatingSquare, Gcd, NonZero, Odd, Resize,
};

use super::unsigned::TeeBigNum;
use crate::error::{CryptoError, Result};

/// Sign for a signed TEE big integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeeBigIntSign {
    /// Negative value.
    Negative,
    /// Zero or positive value.
    Positive,
}

/// A signed big integer for the GP TEE arithmetic API.
#[derive(Debug, Clone)]
pub struct TeeBigInt {
    sign: TeeBigIntSign,
    magnitude: BoxedUint,
}

impl TeeBigInt {
    /// Create a signed zero value.
    pub fn zero() -> Self {
        Self {
            sign: TeeBigIntSign::Positive,
            magnitude: BoxedUint::zero(),
        }
    }

    /// Create a signed one value.
    pub fn one() -> Self {
        Self {
            sign: TeeBigIntSign::Positive,
            magnitude: BoxedUint::one(),
        }
    }

    /// Create a signed integer from sign and big-endian magnitude bytes.
    pub fn from_sign_bytes(sign: i32, bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Err(CryptoError::InvalidInput);
        }
        let mut value = Self {
            sign: if sign < 0 {
                TeeBigIntSign::Negative
            } else {
                TeeBigIntSign::Positive
            },
            magnitude: BoxedUint::from_be_slice_vartime(bytes),
        };
        value.normalize();
        Ok(value)
    }

    /// Create a signed integer from a 32-bit value.
    pub fn from_i32(value: i32) -> Self {
        let sign = if value < 0 {
            TeeBigIntSign::Negative
        } else {
            TeeBigIntSign::Positive
        };
        let magnitude = value.unsigned_abs();
        let mut value = Self {
            sign,
            magnitude: BoxedUint::from(magnitude),
        };
        value.normalize();
        value
    }

    /// Return the sign encoded as `1` or `-1`.
    pub fn sign_i32(&self) -> i32 {
        match self.sign {
            TeeBigIntSign::Negative => -1,
            TeeBigIntSign::Positive => 1,
        }
    }

    /// Return the magnitude as a big-endian byte vector.
    pub fn magnitude_bytes(&self) -> Vec<u8> {
        let bytes = self.magnitude.to_be_bytes_trimmed_vartime();
        if bytes.is_empty() {
            alloc::vec![0]
        } else {
            bytes.into_vec()
        }
    }

    /// Return the magnitude as little-endian 32-bit limbs.
    pub fn magnitude_u32_le(&self) -> Vec<u32> {
        let bytes = self.magnitude.to_le_bytes_trimmed_vartime();
        if bytes.is_empty() {
            return Vec::new();
        }

        let mut limbs = Vec::with_capacity(bytes.len().div_ceil(4));
        for chunk in bytes.chunks(4) {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            limbs.push(u32::from_le_bytes(word));
        }
        while limbs.last().copied() == Some(0) {
            limbs.pop();
        }
        limbs
    }

    /// Build a signed integer from little-endian 32-bit limbs.
    pub fn from_u32_le_limbs(sign: i32, limbs: &[u32]) -> Self {
        let mut bytes = Vec::with_capacity(limbs.len() * 4);
        for limb in limbs {
            bytes.extend_from_slice(&limb.to_le_bytes());
        }
        while bytes.last().copied() == Some(0) {
            bytes.pop();
        }

        let magnitude = if bytes.is_empty() {
            BoxedUint::zero()
        } else {
            BoxedUint::from_le_slice_vartime(&bytes)
        };
        let mut value = Self {
            sign: if sign < 0 {
                TeeBigIntSign::Negative
            } else {
                TeeBigIntSign::Positive
            },
            magnitude,
        };
        value.normalize();
        value
    }

    /// Return true when the value is zero.
    pub fn is_zero(&self) -> bool {
        self.magnitude.is_zero().into()
    }

    /// Return true when the value is negative.
    pub fn is_negative(&self) -> bool {
        self.sign == TeeBigIntSign::Negative && !self.is_zero()
    }

    /// Return the number of significant magnitude bits.
    pub fn bit_length(&self) -> usize {
        self.magnitude.bits_vartime() as usize
    }

    /// Return the number of bytes required by the magnitude.
    pub fn byte_length(&self) -> usize {
        self.bit_length().div_ceil(8).max(1)
    }

    /// Convert to i32 if the value fits.
    pub fn to_i32(&self) -> Result<i32> {
        let bytes = self.magnitude_bytes();
        if bytes.len() > 4 {
            return Err(CryptoError::ArithmeticOverflow);
        }
        let mut buf = [0u8; 4];
        buf[4 - bytes.len()..].copy_from_slice(&bytes);
        let magnitude = u32::from_be_bytes(buf);

        match self.sign {
            TeeBigIntSign::Positive => {
                i32::try_from(magnitude).map_err(|_| CryptoError::ArithmeticOverflow)
            }
            TeeBigIntSign::Negative => {
                if magnitude == 0x8000_0000 {
                    Ok(i32::MIN)
                } else {
                    i32::try_from(magnitude)
                        .map(|value| -value)
                        .map_err(|_| CryptoError::ArithmeticOverflow)
                }
            }
        }
    }

    /// Compare two signed values.
    pub fn compare(&self, other: &Self) -> Ordering {
        match (self.is_negative(), other.is_negative()) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => self.magnitude.cmp(&other.magnitude),
            (true, true) => other.magnitude.cmp(&self.magnitude),
        }
    }

    /// Compare with a signed 32-bit value.
    pub fn compare_i32(&self, value: i32) -> Ordering {
        self.compare(&Self::from_i32(value))
    }

    /// Return the absolute value.
    pub fn abs(&self) -> Self {
        let mut value = self.clone();
        value.sign = TeeBigIntSign::Positive;
        value
    }

    /// Return the negated value.
    pub fn neg(&self) -> Self {
        let mut value = self.clone();
        if !value.is_zero() {
            value.sign = match value.sign {
                TeeBigIntSign::Negative => TeeBigIntSign::Positive,
                TeeBigIntSign::Positive => TeeBigIntSign::Negative,
            };
        }
        value
    }

    /// Add two signed values.
    pub fn add(&self, other: &Self) -> Self {
        let mut value = match (self.is_negative(), other.is_negative()) {
            (false, false) => Self::from_parts(
                TeeBigIntSign::Positive,
                self.magnitude.concatenating_add(&other.magnitude),
            ),
            (true, true) => Self::from_parts(
                TeeBigIntSign::Negative,
                self.magnitude.concatenating_add(&other.magnitude),
            ),
            (false, true) => Self::sub_magnitudes(&self.magnitude, &other.magnitude),
            (true, false) => Self::sub_magnitudes(&other.magnitude, &self.magnitude),
        };
        value.normalize();
        value
    }

    /// Subtract `other` from `self`.
    pub fn sub(&self, other: &Self) -> Self {
        self.add(&other.neg())
    }

    /// Multiply two signed values.
    pub fn mul(&self, other: &Self) -> Self {
        let sign = if self.is_negative() ^ other.is_negative() {
            TeeBigIntSign::Negative
        } else {
            TeeBigIntSign::Positive
        };
        Self::from_parts(sign, self.magnitude.concatenating_mul(&other.magnitude))
    }

    /// Square the signed value.
    pub fn square(&self) -> Self {
        Self::from_parts(
            TeeBigIntSign::Positive,
            self.magnitude.concatenating_square(),
        )
    }

    /// Divide two signed values, returning quotient and remainder.
    pub fn div_rem(&self, divisor: &Self) -> Result<(Self, Self)> {
        if divisor.is_zero() {
            return Err(CryptoError::DivideByZero);
        }
        let divisor_abs = divisor
            .magnitude
            .to_nz()
            .into_option()
            .ok_or(CryptoError::DivideByZero)?;
        let (quotient, remainder) = self.magnitude.div_rem_vartime(&divisor_abs);
        let quotient_sign = if self.is_negative() ^ divisor.is_negative() {
            TeeBigIntSign::Negative
        } else {
            TeeBigIntSign::Positive
        };
        let remainder_sign = if self.is_negative() {
            TeeBigIntSign::Negative
        } else {
            TeeBigIntSign::Positive
        };
        Ok((
            Self::from_parts(quotient_sign, quotient),
            Self::from_parts(remainder_sign, remainder),
        ))
    }

    /// Return `self mod modulus` as a non-negative value.
    pub fn modulo(&self, modulus: &Self) -> Result<Self> {
        let modulus = Self::positive_modulus(modulus)?;
        let modulus_value = modulus.clone().get();
        let remainder = self.magnitude.rem_vartime(&modulus);
        if self.is_negative() && !bool::from(remainder.is_zero()) {
            Ok(Self::from_parts(
                TeeBigIntSign::Positive,
                modulus_value - &remainder,
            ))
        } else {
            Ok(Self::from_parts(TeeBigIntSign::Positive, remainder))
        }
    }

    /// Modular addition.
    pub fn add_mod(&self, other: &Self, modulus: &Self) -> Result<Self> {
        self.add(other).modulo(modulus)
    }

    /// Modular subtraction.
    pub fn sub_mod(&self, other: &Self, modulus: &Self) -> Result<Self> {
        self.sub(other).modulo(modulus)
    }

    /// Modular multiplication.
    pub fn mul_mod(&self, other: &Self, modulus: &Self) -> Result<Self> {
        self.mul(other).modulo(modulus)
    }

    /// Modular square.
    pub fn square_mod(&self, modulus: &Self) -> Result<Self> {
        self.square().modulo(modulus)
    }

    /// Modular inverse.
    pub fn inv_mod(&self, modulus: &Self) -> Result<Self> {
        let modulus = Self::positive_modulus(modulus)?;
        let precision = modulus.bits_precision();
        let modulus_value = modulus.clone().get();
        let base = self
            .modulo(&Self::from_parts(
                TeeBigIntSign::Positive,
                modulus_value.clone(),
            ))?
            .magnitude
            .resize(precision);
        let inverse = base
            .invert_mod(&modulus)
            .into_option()
            .ok_or(CryptoError::InvalidModulus)?;
        Ok(Self::from_parts(TeeBigIntSign::Positive, inverse))
    }

    /// Modular exponentiation. The modulus must be odd.
    pub fn exp_mod(&self, exponent: &Self, modulus: &Self) -> Result<Self> {
        if exponent.is_negative() {
            return Err(CryptoError::InvalidExponent);
        }
        let odd_modulus = Self::positive_odd_modulus(modulus)?;
        let modulus_value = odd_modulus.clone().get();
        let base = self
            .modulo(&Self::from_parts(
                TeeBigIntSign::Positive,
                modulus_value.clone(),
            ))?
            .magnitude
            .resize(odd_modulus.bits_precision());
        Ok(Self::from_parts(
            TeeBigIntSign::Positive,
            base.pow_mod(&exponent.magnitude, &odd_modulus),
        ))
    }

    /// Greatest common divisor of the magnitudes.
    pub fn gcd(&self, other: &Self) -> Self {
        Self::from_parts(
            TeeBigIntSign::Positive,
            self.magnitude.gcd_vartime(&other.magnitude),
        )
    }

    /// Extended greatest common divisor.
    ///
    /// Returns `(gcd, u, v)` where `u * self + v * other == gcd`.
    pub fn extended_gcd(&self, other: &Self) -> (Self, Self, Self) {
        let mut old_r = self.abs();
        let mut r = other.abs();
        let mut old_s = Self::one();
        let mut s = Self::zero();
        let mut old_t = Self::zero();
        let mut t = Self::one();

        while !r.is_zero() {
            let (quotient, remainder) = match old_r.div_rem(&r) {
                Ok(result) => result,
                Err(_) => break,
            };
            old_r = r;
            r = remainder;

            let next_s = old_s.sub(&quotient.mul(&s));
            old_s = s;
            s = next_s;

            let next_t = old_t.sub(&quotient.mul(&t));
            old_t = t;
            t = next_t;
        }

        if self.is_negative() {
            old_s = old_s.neg();
        }
        if other.is_negative() {
            old_t = old_t.neg();
        }

        (old_r.abs(), old_s, old_t)
    }

    /// Return true if the magnitudes are relatively prime.
    pub fn relative_prime(&self, other: &Self) -> bool {
        self.gcd(other).magnitude == BoxedUint::one()
    }

    /// Return true when this positive value is probably prime.
    pub fn is_probable_prime(&self) -> bool {
        !self.is_negative() && crypto_primes::is_prime(crypto_primes::Flavor::Any, &self.magnitude)
    }

    /// Get the magnitude bit at `bit_index`.
    pub fn get_bit(&self, bit_index: u32) -> bool {
        self.magnitude.bit_vartime(bit_index)
    }

    /// Set the magnitude bit at `bit_index`.
    pub fn set_bit(&mut self, bit_index: u32, value: bool) {
        let min_precision = bit_index.saturating_add(1);
        if min_precision > self.magnitude.bits_precision() {
            self.magnitude = self.magnitude.clone().resize(min_precision);
        }
        self.magnitude.set_bit_vartime(bit_index, value);
        self.normalize();
    }

    /// Shift the magnitude right.
    pub fn shr(&self, bits: usize) -> Self {
        let mut value = Self::from_parts(
            self.sign,
            self.magnitude
                .shr_vartime(bits as u32)
                .unwrap_or_else(BoxedUint::zero),
        );
        value.normalize();
        value
    }

    /// Return the unsigned magnitude.
    pub fn magnitude(&self) -> TeeBigNum {
        TeeBigNum::from_boxed(self.magnitude.clone())
    }

    fn from_parts(sign: TeeBigIntSign, magnitude: BoxedUint) -> Self {
        let mut value = Self { sign, magnitude };
        value.normalize();
        value
    }

    fn normalize(&mut self) {
        if self.magnitude.is_zero().into() {
            self.sign = TeeBigIntSign::Positive;
            self.magnitude = BoxedUint::zero();
        }
    }

    fn sub_magnitudes(a: &BoxedUint, b: &BoxedUint) -> Self {
        match a.cmp(b) {
            Ordering::Greater | Ordering::Equal => Self::from_parts(TeeBigIntSign::Positive, a - b),
            Ordering::Less => Self::from_parts(TeeBigIntSign::Negative, b - a),
        }
    }

    fn positive_modulus(modulus: &Self) -> Result<NonZero<BoxedUint>> {
        if modulus.is_negative() || modulus.compare_i32(2) == Ordering::Less {
            return Err(CryptoError::InvalidModulus);
        }
        modulus
            .magnitude
            .to_nz()
            .into_option()
            .ok_or(CryptoError::InvalidModulus)
    }

    fn positive_odd_modulus(modulus: &Self) -> Result<Odd<BoxedUint>> {
        if modulus.is_negative() || modulus.compare_i32(3) == Ordering::Less {
            return Err(CryptoError::InvalidModulus);
        }
        modulus
            .magnitude
            .to_odd()
            .into_option()
            .ok_or(CryptoError::InvalidModulus)
    }
}
