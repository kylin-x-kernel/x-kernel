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
pub(super) fn crypto_rng_read(buf: &mut [u8]) -> TeeResult {
    tee_software_get_rand(buf);
    Ok(())
}

pub(super) struct TeeSoftwareRng {
    rng: DeterministicRng,
}

impl TeeSoftwareRng {
    pub(super) fn new() -> Self {
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
pub(super) struct GlobalSoftwareRng;

impl TryRng for GlobalSoftwareRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        let mut bytes = [0u8; 4];
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: crypto_rng_read fills from the kernel \
                      entropy pool and unconditionally returns Ok"
        )]
        crypto_rng_read(&mut bytes).expect("global RNG failure");
        Ok(u32::from_le_bytes(bytes))
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        let mut bytes = [0u8; 8];
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: crypto_rng_read fills from the kernel \
                      entropy pool and unconditionally returns Ok"
        )]
        crypto_rng_read(&mut bytes).expect("global RNG failure");
        Ok(u64::from_le_bytes(bytes))
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: crypto_rng_read fills from the kernel \
                      entropy pool and unconditionally returns Ok"
        )]
        crypto_rng_read(dest).expect("global RNG failure");
        Ok(())
    }
}

impl TryCryptoRng for GlobalSoftwareRng {}

unittest::path_mod_test!(pub(super) mod tests_rng_software, "../../ut-tests/tee/rng_software/tests_rng_software.rs");
