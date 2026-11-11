// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use core::cell::RefCell;

use raw_cpuid::{CpuIdReader, CpuIdResult};
use unittest::{assert, assert_eq, def_test};

use super::{CpuFeature, CpuInfo, StateSupport};

fn leaf(eax: u32, ebx: u32, ecx: u32, edx: u32) -> CpuIdResult {
    CpuIdResult { eax, ebx, ecx, edx }
}

fn all_features(number: u32, subleaf: u32) -> CpuIdResult {
    match number {
        0 => leaf(
            7,
            u32::from_le_bytes(*b"Genu"),
            u32::from_le_bytes(*b"ntel"),
            u32::from_le_bytes(*b"ineI"),
        ),
        1 => leaf(0x0005_0654, 0, u32::MAX, u32::MAX),
        7 if subleaf == 0 => leaf(0, u32::MAX, u32::MAX, u32::MAX),
        0x8000_0000 => leaf(0x8000_0000, 0, 0, 0),
        _ => leaf(0, 0, 0, 0),
    }
}

fn identity_reader(vendor: [u8; 12], brand: [u8; 48], signature: u32) -> impl CpuIdReader {
    move |number, subleaf| {
        let word = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap());
        match number {
            0 => leaf(
                7,
                word(&vendor[..4]),
                word(&vendor[8..]),
                word(&vendor[4..8]),
            ),
            1 => leaf(signature, 0, u32::MAX, u32::MAX),
            0x8000_0000 => leaf(0x8000_0004, 0, 0, 0),
            0x8000_0002..=0x8000_0004 => {
                let offset = (number - 0x8000_0002) as usize * 16;
                let words: [u32; 4] =
                    core::array::from_fn(|i| word(&brand[offset + 4 * i..offset + 4 * i + 4]));
                leaf(words[0], words[1], words[2], words[3])
            }
            _ => all_features(number, subleaf),
        }
    }
}

#[def_test]
fn reports_identity_and_sse3_with_fxsave() {
    let info = CpuInfo::detect(all_features, StateSupport::Fxsave);
    assert_eq!(info.vendor_id(), "GenuineIntel");
    assert_eq!(info.model_name(), "unknown");
    assert_eq!(info.family_model_stepping(), Some((6, 85, 4)));
    for feature in [
        CpuFeature::Sse3,
        CpuFeature::Ssse3,
        CpuFeature::Sse4_1,
        CpuFeature::Sse4_2,
        CpuFeature::Bmi2,
        CpuFeature::Rdpid,
    ] {
        assert!(info.has_feature(feature));
    }
}

#[def_test]
fn hardware_xsave_and_avx_do_not_imply_kernel_support() {
    let info = CpuInfo::detect(all_features, StateSupport::Fxsave);
    for feature in [
        CpuFeature::Xsave,
        CpuFeature::Avx,
        CpuFeature::Avx2,
        CpuFeature::Fma,
        CpuFeature::F16c,
    ] {
        assert!(!info.has_feature(feature));
    }
}

#[def_test]
fn missing_state_management_masks_fpu_and_simd() {
    let info = CpuInfo::detect(all_features, StateSupport::Disabled);
    for feature in [
        CpuFeature::Fpu,
        CpuFeature::Mmx,
        CpuFeature::Fxsr,
        CpuFeature::Sse,
        CpuFeature::Sse2,
        CpuFeature::Sse3,
        CpuFeature::Pclmulqdq,
        CpuFeature::Ssse3,
        CpuFeature::Sse4_1,
        CpuFeature::Sse4_2,
        CpuFeature::Aes,
        CpuFeature::Sha,
    ] {
        assert!(!info.has_feature(feature));
    }
    assert!(info.has_feature(CpuFeature::Bmi1));
    assert!(info.has_feature(CpuFeature::Hypervisor));
}

#[def_test]
fn missing_leaves_are_not_queried_or_reported() {
    let queried = RefCell::new(alloc::vec::Vec::new());
    let info = CpuInfo::detect(
        |number, subleaf| {
            queried.borrow_mut().push((number, subleaf));
            leaf(0, 0, 0, 0)
        },
        StateSupport::Fxsave,
    );
    assert!(queried.borrow().contains(&(0, 0)));
    assert!(queried.borrow().contains(&(0x8000_0000, 0)));
    assert!(
        queried
            .borrow()
            .iter()
            .all(|query| matches!(query, (0 | 0x8000_0000, 0)))
    );
    assert_eq!(info.vendor_id(), "unknown");
    assert_eq!(info.family_model_stepping(), None);
    assert!(!info.has_feature(CpuFeature::Sse3));
    assert!(!info.has_feature(CpuFeature::Bmi1));
    assert!(!info.has_feature(CpuFeature::Rdpid));
}

#[def_test]
fn missing_fxsr_or_sse_clears_dependent_simd_features() {
    for bit in [24, 25, 26] {
        let info = CpuInfo::detect(
            |number, subleaf| {
                let mut result = all_features(number, subleaf);
                if number == 1 {
                    result.edx &= !(1 << bit);
                }
                result
            },
            StateSupport::Fxsave,
        );
        assert!(!info.has_feature(CpuFeature::Sse3));
        assert!(!info.has_feature(CpuFeature::Sha));
    }
}

#[def_test]
fn amd_extended_signature_and_brand_are_decoded() {
    let mut brand = [b' '; 48];
    brand[2..10].copy_from_slice(b"Test CPU");
    let info = CpuInfo::detect(
        identity_reader(
            *b"AuthenticAMD",
            brand,
            (10 << 20) | (2 << 16) | (15 << 8) | (1 << 4) | 2,
        ),
        StateSupport::Disabled,
    );
    assert_eq!(info.vendor_id(), "AuthenticAMD");
    assert_eq!(info.model_name(), "Test CPU");
    assert_eq!(info.family_model_stepping(), Some((25, 33, 2)));
}

#[def_test]
fn family_model_decoding_obeys_vendor_rules() {
    for (vendor, base_family, extended_family, expected) in [
        (*b"GenuineIntel", 6, 3, (6, 33, 2)),
        (*b"AuthenticAMD", 6, 3, (6, 1, 2)),
        (*b"HygonGenuine", 6, 3, (6, 1, 2)),
        (*b"GenuineIntel", 15, 10, (25, 33, 2)),
        (*b"AuthenticAMD", 15, 10, (25, 33, 2)),
        (*b"TestVendor12", 6, 3, (9, 33, 2)),
    ] {
        let signature = (extended_family << 20) | (2 << 16) | (base_family << 8) | (1 << 4) | 2;
        let info = CpuInfo::detect(
            identity_reader(vendor, [0; 48], signature),
            StateSupport::Fxsave,
        );
        assert_eq!(info.family_model_stepping(), Some(expected));
    }
    let info = CpuInfo::detect(
        identity_reader(*b"AuthenticAMD", [0; 48], (255 << 20) | (15 << 8)),
        StateSupport::Fxsave,
    );
    assert_eq!(info.family_model_stepping(), None);
}

#[def_test]
fn truncated_leaf_ranges_do_not_produce_features_or_brand() {
    for basic_max in [1, 6, 7] {
        for extended_max in [0, 0x8000_0000, 0x8000_0002, 0x8000_0003] {
            let queried = RefCell::new(alloc::vec::Vec::new());
            let info = CpuInfo::detect(
                |number, subleaf| {
                    queried.borrow_mut().push((number, subleaf));
                    let mut result = all_features(number, subleaf);
                    if number == 0 {
                        result.eax = basic_max;
                    }
                    if number == 0x8000_0000 {
                        result.eax = extended_max;
                    }
                    result
                },
                StateSupport::Avx,
            );
            assert_eq!(info.model_name(), "unknown");
            assert_eq!(info.has_feature(CpuFeature::Avx2), basic_max >= 7);
            assert!(
                queried
                    .borrow()
                    .iter()
                    .all(|&(number, _)| number == 0x8000_0000 || number <= basic_max)
            );
        }
    }
}

#[def_test]
fn raw_bitmap_keeps_register_order_and_unnamed_bits() {
    let expected = [
        0xa5a5_a5a5 | 1 | (7 << 24),
        0x5a5a_5a5a | (1 << 26) | (1 << 28),
        0xa55a_6996,
        0x5aa5_9669,
    ];
    let info = CpuInfo::detect(
        |number, subleaf| match (number, subleaf) {
            (1, 0) => leaf(0, 0, expected[1], expected[0]),
            (7, 0) => leaf(1, expected[2], expected[3], 0),
            (7, 1) => leaf(0, !expected[2], !expected[3], 0),
            _ => all_features(number, subleaf),
        },
        StateSupport::Avx,
    );
    assert_eq!(info.capabilities, expected);
}

#[def_test]
fn identity_decoding_rejects_malformed_strings_before_library_normalization() {
    for byte in [b'\n', b'\t', 0xff] {
        let mut vendor = *b"GenuineIntel";
        vendor[4] = byte;
        let info = CpuInfo::detect(identity_reader(vendor, [0; 48], 0), StateSupport::Fxsave);
        assert_eq!(info.vendor_id(), "unknown");
        for offset in [0, 16, 47] {
            let mut brand = [b'X'; 48];
            brand[offset] = byte;
            let info = CpuInfo::detect(
                identity_reader(*b"GenuineIntel", brand, 0),
                StateSupport::Fxsave,
            );
            assert_eq!(info.model_name(), "unknown");
        }
    }
    let mut brand = [0xff; 48];
    brand[..10].copy_from_slice(b" Test CPU\0");
    let info = CpuInfo::detect(
        identity_reader(*b"GenuineIntel", brand, 0),
        StateSupport::Fxsave,
    );
    assert_eq!(info.model_name(), "Test CPU");
    let info = CpuInfo::detect(
        identity_reader(*b"GenuineIntel", [b'X'; 48], 0),
        StateSupport::Fxsave,
    );
    assert_eq!(
        info.model_name(),
        "XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX"
    );
}

#[def_test]
fn cpuid_text_cannot_inject_lines_or_invalid_utf8() {
    assert_eq!(super::cpuid_text(b"CPU\nflags: avx"), "unknown");
    assert_eq!(super::cpuid_text(b"\xffCPU"), "unknown");
    assert_eq!(super::cpuid_text(b" CPU \0ignored"), "CPU");
    assert_eq!(super::cpuid_text(b"   "), "unknown");
}

#[def_test]
fn boot_cpu_publishes_its_own_identity() {
    // Pin the CPUID probe and the logical-ID lookup to the same CPU. Restore
    // IRQs before assertions, which can return early from a failed test.
    let irq_state = karch::save_irq_and_disable();
    let cpu_id = kcpu_slot::current_area().map(|area| area.cpu());
    let leaf = core::arch::x86_64::__cpuid(0);
    let features = core::arch::x86_64::__cpuid(1);
    karch::restore_irq(irq_state);
    let info = crate::cpu_info(cpu_id.unwrap()).unwrap();
    if cfg!(feature = "fp-simd") {
        assert_eq!(info.has_feature(CpuFeature::Sse3), features.ecx & 1 != 0);
    }
    let mut vendor = [0u8; 12];
    vendor[..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&leaf.edx.to_le_bytes());
    vendor[8..].copy_from_slice(&leaf.ecx.to_le_bytes());
    assert_eq!(info.vendor_id(), super::cpuid_text(&vendor));
    assert!(crate::cpu_info(kcpu_id_map::LogicalCpuId::new(kbuild_config::NR_CPUS)).is_none());
}

#[def_test]
fn usable_avx_state_preserves_hardware_flags() {
    let info = CpuInfo::detect(all_features, StateSupport::Avx);
    for feature in [
        CpuFeature::Xsave,
        CpuFeature::Avx,
        CpuFeature::Avx2,
        CpuFeature::Fma,
        CpuFeature::F16c,
    ] {
        assert!(info.has_feature(feature));
    }
    let info = CpuInfo::detect(all_features, StateSupport::Xsave);
    assert!(info.has_feature(CpuFeature::Xsave));
    assert!(!info.has_feature(CpuFeature::Avx));
    assert!(!info.has_feature(CpuFeature::Avx2));
    let info = CpuInfo::detect(
        |number, subleaf| {
            let mut leaf = all_features(number, subleaf);
            if number == 1 {
                leaf.ecx &= !(1 << 28);
            }
            leaf
        },
        StateSupport::Avx,
    );
    assert!(!info.has_feature(CpuFeature::Avx2));
}
