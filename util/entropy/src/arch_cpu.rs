// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture CPU instruction entropy (AArch64 RNDR, x86_64 RDSEED/RDRAND).

use alloc::vec::Vec;

/// Probe and log CPU RNG availability.
pub(crate) fn init() {
    if kbuild_config::KFEAT_ENTROPY_ARCH_CPU {
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        {
            karch::init_cpu_rng();
            if karch::cpu_rng_available() {
                #[cfg(target_arch = "aarch64")]
                log::info!("entropy: AArch64 CPU RNDR available");
                #[cfg(target_arch = "x86_64")]
                log::info!("entropy: x86_64 CPU RDSEED/RDRAND available");
            }
        }
    }
}

/// Returns whether CPU instruction RNG is enabled and available on this CPU.
pub(crate) fn is_available() -> bool {
    if !kbuild_config::KFEAT_ENTROPY_ARCH_CPU {
        return false;
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        karch::cpu_rng_available()
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

/// Read up to `len` bytes from the CPU RNG into a new buffer.
pub(crate) fn read(len: usize) -> Option<Vec<u8>> {
    if len == 0 || !is_available() {
        return None;
    }

    let mut buf = alloc::vec![0u8; len];
    let read = read_into(&mut buf);
    if read == 0 {
        return None;
    }
    buf.truncate(read);
    Some(buf)
}

fn read_into(buf: &mut [u8]) -> usize {
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        karch::read_cpu_random(buf)
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = buf;
        0
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, assert_ne, def_test};

    use super::*;

    #[def_test]
    fn test_arch_cpu_read_zero_len() {
        assert!(read(0).is_none());
    }

    #[def_test]
    fn test_arch_cpu_disabled_without_kconfig() {
        if !kbuild_config::KFEAT_ENTROPY_ARCH_CPU {
            assert!(!is_available());
            assert!(read(16).is_none());
        }
    }

    #[def_test]
    fn test_arch_cpu_read_when_available() {
        init();
        if is_available() {
            let data = read(32).expect("CPU RNG should return bytes when available");
            assert_eq!(data.len(), 32);
            assert_ne!(data.as_slice(), &[0u8; 32]);
        } else {
            assert!(read(32).is_none());
        }
    }
}
