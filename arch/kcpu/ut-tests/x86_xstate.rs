// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn simulated_cpuid(leaf: u32, subleaf: u32) -> CpuidResult {
    match (leaf, subleaf) {
        (0, 0) => CpuidResult {
            eax: 0xd,
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
        (1, 0) => CpuidResult {
            eax: 0,
            ebx: 0,
            ecx: (1 << 26) | (1 << 28),
            edx: 0,
        },
        (0xd, 0) => CpuidResult {
            eax: 7,
            ebx: 832,
            ecx: 832,
            edx: 0,
        },
        (0xd, 2) => CpuidResult {
            eax: 256,
            ebx: 576,
            ecx: 0,
            edx: 0,
        },
        _ => CpuidResult {
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: 0,
        },
    }
}

#[def_test]
fn xstate_discovery_checks_ymm_layout_and_dependencies() {
    assert_eq!(supported_mask(simulated_cpuid), AVX_STATE);
    assert_eq!(
        supported_mask(|leaf, subleaf| {
            let mut result = simulated_cpuid(leaf, subleaf);
            if leaf == 0xd && subleaf == 2 {
                result.ebx = 1024;
            }
            result
        }),
        FPSSE
    );
    assert_eq!(
        supported_mask(|leaf, subleaf| {
            let mut result = simulated_cpuid(leaf, subleaf);
            if leaf == 1 {
                result.ecx &= !(1 << 26);
            }
            result
        }),
        0
    );
}

#[def_test]
fn signal_import_bounds_and_validates_xstate() {
    let (bytes, len) = ExtendedState::capture().signal_bytes();
    assert!(ExtendedState::from_signal_bytes(&bytes[..len]).is_ok());
    assert!(ExtendedState::from_signal_bytes(&bytes[..511]).is_err());
    if enabled_xstate_mask() != 0 {
        for offset in [512, 520, 528, len - 4] {
            let mut invalid = bytes;
            invalid[offset] ^= 0x80;
            assert!(ExtendedState::from_signal_bytes(&invalid[..len]).is_err());
        }
        let mut invalid = bytes;
        invalid[480..484].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(signal_fp_length(&invalid).is_err());
    }
    let mut legacy = [0; 512];
    legacy.copy_from_slice(&bytes[..512]);
    legacy[464..512].fill(0);
    let decoded = ExtendedState::from_signal_bytes(&legacy).unwrap();
    assert_eq!(decoded.xstate_bv, FPSSE);
    assert_eq!(decoded.ymm_hi, [[0; 2]; 16]);
}

#[cfg(feature = "fp-simd")]
struct RestoreOnDrop(ExtendedState);
#[cfg(feature = "fp-simd")]
impl Drop for RestoreOnDrop {
    fn drop(&mut self) {
        self.0.restore();
    }
}

#[cfg(feature = "fp-simd")]
fn write_ymm0(value: &[u64; 4]) {
    // SAFETY: tests call only with AVX enabled. value contains 32 readable
    // bytes and xmm0 clobber covers its aliased YMM register.
    unsafe {
        core::arch::asm!("vmovdqu ymm0, [{ptr}]", ptr = in(reg) value.as_ptr(), out("xmm0") _, options(nostack));
    }
}

#[cfg(feature = "fp-simd")]
fn read_ymm0() -> [u64; 4] {
    let mut value = [0; 4];
    // SAFETY: AVX is enabled and value owns 32 writable bytes.
    unsafe {
        core::arch::asm!("vmovdqu [{ptr}], ymm0", ptr = in(reg) value.as_mut_ptr(), options(nostack));
    }
    value
}

#[cfg(feature = "fp-simd")]
#[def_test]
fn ymm_survives_save_restore_and_nested_signal_snapshots() {
    if enabled_xstate_mask() & YMM != 0 {
        let _original = RestoreOnDrop(ExtendedState::capture());
        let pattern = [0x11, 0x22, 0xdead_beef, 0xfeed_face];
        write_ymm0(&pattern);
        let interrupted = ExtendedState::capture();
        write_ymm0(&[1, 2, 3, 4]);
        let handler = ExtendedState::capture();
        write_ymm0(&[0; 4]);
        handler.restore();
        assert_eq!(read_ymm0(), [1, 2, 3, 4]);
        let (bytes, len) = interrupted.signal_bytes();
        ExtendedState::from_signal_bytes(&bytes[..len])
            .unwrap()
            .restore();
        assert_eq!(read_ymm0(), pattern);
        ExtendedState::default().restore();
        assert_eq!(read_ymm0(), [0; 4]);
    }
}
