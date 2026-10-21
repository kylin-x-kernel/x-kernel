// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::c_ulong;

use ktask::KCpuMask;
use unittest::{assert_eq, def_test};

use super::NativeCpuSet;

#[def_test]
fn native_cpuset_preserves_empty_sparse_and_full_masks() {
    for pattern in 0..4 {
        let mut original = KCpuMask::new();
        for cpu in 0..kbuild_config::NR_CPUS {
            original.set(
                cpu,
                match pattern {
                    0 => false,
                    1 => cpu == kbuild_config::NR_CPUS - 1,
                    2 => cpu % 2 == 0,
                    _ => true,
                },
            );
        }
        let native = NativeCpuSet::from(&original);
        let restored = KCpuMask::from(&native);
        for cpu in 0..kbuild_config::NR_CPUS {
            assert_eq!(restored.get(cpu), original.get(cpu));
        }
    }
}

#[def_test]
fn native_cpuset_uses_native_words_and_zero_padding() {
    let mut original = KCpuMask::new();
    original.set(0, true);
    original.set(kbuild_config::NR_CPUS - 1, true);
    let native = NativeCpuSet::from(&original);
    let bytes = native.to_ne_bytes();
    assert_eq!(
        bytes.len(),
        kbuild_config::NR_CPUS.div_ceil(c_ulong::BITS as usize)
            * core::mem::size_of::<c_ulong>()
    );
    // Build each expected word independently, including all unused bits.
    for (word_index, chunk) in bytes.chunks_exact(NativeCpuSet::WORD_BYTES).enumerate() {
        let mut expected: c_ulong = 0;
        for bit in 0..c_ulong::BITS as usize {
            let cpu = word_index * c_ulong::BITS as usize + bit;
            if cpu < kbuild_config::NR_CPUS && original.get(cpu) {
                expected |= (1 as c_ulong) << bit;
            }
        }
        let expected_bytes = expected.to_ne_bytes();
        assert_eq!(chunk, expected_bytes.as_slice());
    }
}
