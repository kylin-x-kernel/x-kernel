// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TEE software RNG backed by the kernel entropy pool.

use entropy::fill_random;
use tee_crypto::rng::{DeterministicRng, Infallible, Rng, TryCryptoRng, TryRng};

use crate::tee::TeeResult;

const RNG_SEED_SIZE: usize = 32;

fn tee_software_get_rand(output: &mut [u8]) {
    fill_random(output);
}

fn software_rng_seed() -> [u8; RNG_SEED_SIZE] {
    let mut seed = [0u8; RNG_SEED_SIZE];
    tee_software_get_rand(&mut seed);
    seed
}

/// Read data from the kernel entropy pool into `buf`.
pub fn crypto_rng_read(buf: &mut [u8]) -> TeeResult {
    tee_software_get_rand(buf);
    Ok(())
}

pub struct TeeSoftwareRng {
    rng: DeterministicRng,
}

impl TeeSoftwareRng {
    pub fn new() -> Self {
        let seed = software_rng_seed();
        Self {
            rng: DeterministicRng::seed_from_bytes(&seed),
        }
    }
}

impl TryRng for TeeSoftwareRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.rng.next_u32())
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.rng.next_u64())
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        self.rng.fill_bytes(dest);
        Ok(())
    }
}

impl TryCryptoRng for TeeSoftwareRng {}

/// RNG backed by the kernel entropy pool.
pub struct GlobalSoftwareRng;

impl TryRng for GlobalSoftwareRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut bytes = [0u8; 4];
        crypto_rng_read(&mut bytes).expect("global RNG failure");
        Ok(u32::from_le_bytes(bytes))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut bytes = [0u8; 8];
        crypto_rng_read(&mut bytes).expect("global RNG failure");
        Ok(u64::from_le_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        crypto_rng_read(dest).expect("global RNG failure");
        Ok(())
    }
}

impl TryCryptoRng for GlobalSoftwareRng {}

#[unittest::mod_test]
pub mod tests_rng_software {
    use unittest::assert_ne;

    use super::*;

    #[unittest::def_test]
    fn test_get_rand() {
        let mut buf1 = [0u8; 10];
        let mut buf2 = [0u8; 10];
        tee_software_get_rand(&mut buf1);
        tee_software_get_rand(&mut buf2);
        assert_ne!(buf1, buf2);
    }

    #[unittest::def_test]
    fn test_tee_software_rng() {
        let mut rng = TeeSoftwareRng::new();
        let mut buf = [0u8; 10];
        rng.rng.fill_bytes(&mut buf);
        assert_ne!(buf, [0u8; 10]);
    }

    #[unittest::def_test]
    fn test_tee_software_rng_crypto_rng() {
        let mut rng = TeeSoftwareRng::new();
        let mut buf = [0u8; 10];
        Rng::fill_bytes(&mut rng, &mut buf);
        assert_ne!(buf, [0u8; 10]);
    }

    #[unittest::def_test]
    fn test_tee_software_rng_instances_use_distinct_seeds() {
        let mut rng1 = TeeSoftwareRng::new();
        let mut rng2 = TeeSoftwareRng::new();
        let mut buf1 = [0u8; 32];
        let mut buf2 = [0u8; 32];
        rng1.rng.fill_bytes(&mut buf1);
        rng2.rng.fill_bytes(&mut buf2);
        assert_ne!(buf1, buf2);
    }
}
