// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RNG abstraction — ChaCha20-based cryptographic random number generator.

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
pub use rand_core::{Infallible, Rng, TryCryptoRng, TryRng};

/// Trait for cryptographic random number generation.
pub trait CryptoRng: rand_core::CryptoRng {}

impl<T> CryptoRng for T where T: rand_core::CryptoRng + ?Sized {}

/// ChaCha20-based deterministic RNG for tests and reproducible validation.
pub struct DeterministicRng {
    inner: ChaCha20Rng,
}

impl DeterministicRng {
    /// Create a new RNG seeded from a u64 value.
    pub fn seed_from_u64(seed: u64) -> Self {
        Self {
            inner: ChaCha20Rng::seed_from_u64(seed),
        }
    }

    /// Create a new RNG seeded from a 32-byte seed.
    pub fn seed_from_bytes(seed: &[u8; 32]) -> Self {
        Self {
            inner: ChaCha20Rng::from_seed(*seed),
        }
    }
}

impl TryRng for DeterministicRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> core::result::Result<u32, Self::Error> {
        Ok(self.inner.next_u32())
    }

    fn try_next_u64(&mut self) -> core::result::Result<u64, Self::Error> {
        Ok(self.inner.next_u64())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), Self::Error> {
        self.inner.fill_bytes(dest);
        Ok(())
    }
}

impl TryCryptoRng for DeterministicRng {}

pub(crate) struct RngAdapter<'a> {
    inner: &'a mut dyn CryptoRng,
}

impl<'a> RngAdapter<'a> {
    pub(crate) fn new(inner: &'a mut dyn CryptoRng) -> Self {
        Self { inner }
    }
}

impl TryRng for RngAdapter<'_> {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> core::result::Result<u32, Self::Error> {
        Ok(self.inner.next_u32())
    }

    fn try_next_u64(&mut self) -> core::result::Result<u64, Self::Error> {
        Ok(self.inner.next_u64())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> core::result::Result<(), Self::Error> {
        self.inner.fill_bytes(dest);
        Ok(())
    }
}

impl TryCryptoRng for RngAdapter<'_> {}
