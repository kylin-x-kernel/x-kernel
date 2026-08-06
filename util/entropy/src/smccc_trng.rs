// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SMCCC firmware TRNG entropy (ARM TRNG SMCCC 1.0).

use alloc::vec::Vec;

#[cfg(target_arch = "aarch64")]
mod imp {
    use super::Vec;

    pub(crate) fn init() {
        if !kbuild_config::KFEAT_ENTROPY_SMCCC_TRNG {
            return;
        }

        kplat_aarch64::peripherals::trng::init_trng();
    }

    pub(crate) fn is_available() -> bool {
        if !kbuild_config::KFEAT_ENTROPY_SMCCC_TRNG {
            return false;
        }

        kplat_aarch64::peripherals::trng::trng_available()
    }

    pub(crate) fn read(len: usize) -> Option<Vec<u8>> {
        if len == 0 || !is_available() {
            return None;
        }

        let mut buf = alloc::vec![0u8; len];
        let read = kplat_aarch64::peripherals::trng::read_trng_random(&mut buf);
        if read == 0 {
            return None;
        }
        buf.truncate(read);
        Some(buf)
    }
}

#[cfg(not(target_arch = "aarch64"))]
mod imp {
    use super::Vec;

    pub(crate) fn init() {}

    pub(crate) fn is_available() -> bool {
        false
    }

    pub(crate) fn read(_len: usize) -> Option<Vec<u8>> {
        None
    }
}

pub(crate) use imp::{init, is_available, read};

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;

    #[def_test]
    fn test_smccc_trng_read_zero_len() {
        assert!(read(0).is_none());
    }

    // `KFEAT_ENTROPY_SMCCC_TRNG` is only emitted under ARCH_AARCH64.
    #[cfg(target_arch = "aarch64")]
    #[def_test]
    fn test_smccc_trng_disabled_without_kconfig() {
        if !kbuild_config::KFEAT_ENTROPY_SMCCC_TRNG {
            assert!(!is_available());
            assert!(read(16).is_none());
        }
    }

    #[cfg(not(target_arch = "aarch64"))]
    #[def_test]
    fn test_smccc_trng_unavailable_on_non_aarch64() {
        assert!(!is_available());
        assert!(read(16).is_none());
    }

    #[def_test]
    fn test_smccc_trng_read_when_available() {
        init();
        if is_available() {
            let data = read(32).expect("SMCCC TRNG should return bytes when available");
            assert_eq!(data.len(), 32);
            assert_ne!(data.as_slice(), &[0u8; 32]);
        } else {
            assert!(read(32).is_none());
        }
    }
}
