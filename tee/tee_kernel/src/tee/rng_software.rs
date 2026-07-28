// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use klazy::Lazy;
use ksync::Mutex;
use kvfs::{Filename, NodePermission};
use macros::register_init;
use tee_crypto::rng::{DeterministicRng, Infallible, Rng, TryCryptoRng, TryRng};
use tee_raw_sys::TEE_ERROR_GENERIC;

use crate::tee::TeeResult;

const RNG_SEED_SIZE: usize = 32;

static GLOBAL_TEE_SOFTWARE_RAND: Lazy<Mutex<DeterministicRng>> = Lazy::new(|| {
    let seed = kernel_random_seed().expect("TEE software RNG seed unavailable");
    Mutex::new(DeterministicRng::seed_from_bytes(&seed))
});

fn tee_software_get_rand(output: &mut [u8]) {
    let mut rand = GLOBAL_TEE_SOFTWARE_RAND.lock();
    rand.fill_bytes(output);
}

/// Eagerly initialize the global TEE software RNG at boot.
///
/// Registered via `#[register_init]`, this runs during `init_cb()` after devfs
/// is mounted and before any task can issue a TEE syscall. Reading `/dev/urandom`
/// here -- on a single CPU with no concurrent RNG caller alive -- cannot contend.
/// Once `Ready`, every later `tee_software_get_rand` takes a fast non-blocking
/// path and never touches the VFS, so the factory closure's `/dev/urandom` read
/// (which acquires the sleeping `fs_context`/devfs mutexes) can no longer run
/// inside a `klazy::Once` spin-wait or under a held TEE-object lock. That removes
/// the SMP AB-BA inversion with the TEE storage path that hung the system under
/// contention.
#[register_init]
fn init_tee_software_rand() {
    Lazy::force(&GLOBAL_TEE_SOFTWARE_RAND);
}

fn kernel_random_seed() -> TeeResult<[u8; RNG_SEED_SIZE]> {
    let mut seed = [0u8; RNG_SEED_SIZE];
    let fs_guard = fs_context::init_fs();
    let fs = fs_guard.lock();
    let file = Filename::new("/dev/urandom")
        .open_with_flags_at(
            fs.root(),
            fs.pwd(),
            0,
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .map_err(|_| TEE_ERROR_GENERIC)?;
    drop(fs);

    let mut pos = 0;
    let len = file
        .read_from(&mut seed, &mut pos)
        .map_err(|_| TEE_ERROR_GENERIC)?;
    if len != seed.len() {
        return Err(TEE_ERROR_GENERIC);
    }
    Ok(seed)
}

fn software_rng_seed() -> [u8; RNG_SEED_SIZE] {
    let mut seed = [0u8; RNG_SEED_SIZE];
    tee_software_get_rand(&mut seed);
    seed
}

/// read data from crypto RNG to buffer
///
/// # Arguments
/// * `buf` - buffer to store read data
///
/// # Returns
/// * `Ok(())` - success
/// * `Err(TEE_ERROR_GENERIC)` - error
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

/// RNG backed by the global `GLOBAL_TEE_SOFTWARE_RAND`.
///
/// This reuses the persistent global CSPRNG so state is continuous across
/// calls.
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
