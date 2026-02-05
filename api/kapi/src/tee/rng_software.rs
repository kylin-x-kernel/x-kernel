// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ksync::Mutex;
use ktypes::Lazy;
use mbedtls::{pk::Pk, rng::RngCallback};
use mbedtls_sys_auto::types::{
    raw_types::{c_int, c_uchar, c_void},
    size_t,
};
use rand_chacha::{
    ChaCha20Rng,
    rand_core::{RngCore, SeedableRng},
};

use crate::tee::TeeResult;

static GLOBAL_TEE_SOFTWARE_RAND: Lazy<Mutex<ChaCha20Rng>> = Lazy::new(|| {
    let seed = khal::time::now_ticks();
    Mutex::new(ChaCha20Rng::seed_from_u64(seed))
});

fn tee_software_get_rand(output: &mut [u8]) {
    let mut rand = GLOBAL_TEE_SOFTWARE_RAND.lock();
    rand.fill_bytes(output);
}

/// read data from crypto RNG to buffer
///
/// # Arguments
/// * `buf` - buffer to store read data
///
/// # Returns
/// * `Ok(())` - success
/// * `Err(TEE_ERROR_GENERIC)` - error
///   TODO: Using mbedtls to implement a real RNG
pub fn crypto_rng_read(buf: &mut [u8]) -> TeeResult {
    // buf.fill(0);
    tee_software_get_rand(buf);
    Ok(())
}

pub struct TeeSoftwareRng {
    rng: ChaCha20Rng,
}

impl TeeSoftwareRng {
    pub fn new() -> Self {
        let seed = khal::time::now_ticks();
        Self {
            rng: ChaCha20Rng::seed_from_u64(seed),
        }
    }
}

impl RngCallback for TeeSoftwareRng {
    unsafe extern "C" fn call(p_rng: *mut c_void, data: *mut c_uchar, len: size_t) -> c_int {
        let rng = unsafe { &mut *(p_rng as *mut TeeSoftwareRng) };
        rng.rng
            .fill_bytes(unsafe { core::slice::from_raw_parts_mut(data, len) });
        0
    }

    fn data_ptr(&self) -> *mut c_void {
        self as *const _ as *mut _
    }
}

#[cfg(feature = "tee_test")]
pub mod tests_rng_software {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_get_rand() {
            let mut buf1 = [0u8; 10];
            let mut buf2 = [0u8; 10];
            tee_software_get_rand(&mut buf1);
            tee_software_get_rand(&mut buf2);
            assert_ne!(buf1, buf2);
        }
    }

    test_fn! {
        using TestResult;
        fn test_tee_software_rng() {
            let rng = TeeSoftwareRng::new();
            let mut buf = [0u8; 10];
            unsafe { TeeSoftwareRng::call(rng.data_ptr(), buf.as_mut_ptr(), buf.len()) };
            assert_ne!(buf, [0u8; 10]);
        }
    }

    tests_name! {
        TEST_RNG_SOFTWARE;
        //------------------------
        rng_software;
        test_get_rand,
        test_tee_software_rng,
    }
}
