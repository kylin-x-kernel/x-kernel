// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Native unsigned-long CPU-set layout at the Linux syscall boundary.
//!
//! Keep this ABI representation separate from KCpuMask's compact storage.
//! Each syscall retains its own input-length and user-copy error policy.

use core::ffi::c_ulong;

use ktask::KCpuMask;

const WORD_BITS: usize = c_ulong::BITS as usize;
const WORD_COUNT: usize = kbuild_config::NR_CPUS.div_ceil(WORD_BITS);

pub(crate) struct NativeCpuSet {
    words: [c_ulong; WORD_COUNT],
}

impl NativeCpuSet {
    pub(crate) const BYTE_LEN: usize = WORD_COUNT * Self::WORD_BYTES;
    pub(crate) const WORD_BYTES: usize = core::mem::size_of::<c_ulong>();

    pub(crate) fn to_ne_bytes(&self) -> [u8; Self::BYTE_LEN] {
        let mut bytes = [0u8; Self::BYTE_LEN];
        for (chunk, word) in bytes.chunks_exact_mut(Self::WORD_BYTES).zip(self.words) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        bytes
    }
}

impl From<&KCpuMask> for NativeCpuSet {
    fn from(mask: &KCpuMask) -> Self {
        let mut words = [0; WORD_COUNT];
        for cpu in 0..kbuild_config::NR_CPUS {
            if mask.get(cpu) {
                words[cpu / WORD_BITS] |= (1 as c_ulong) << (cpu % WORD_BITS);
            }
        }
        Self { words }
    }
}

impl From<&NativeCpuSet> for KCpuMask {
    fn from(mask: &NativeCpuSet) -> Self {
        let mut cpus = Self::new();
        for cpu in 0..kbuild_config::NR_CPUS {
            cpus.set(
                cpu,
                mask.words[cpu / WORD_BITS] & ((1 as c_ulong) << (cpu % WORD_BITS)) != 0,
            );
        }
        cpus
    }
}

unittest::path_mod_test!(tests, "../ut-tests/cpuset/tests.rs");
