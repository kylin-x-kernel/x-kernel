// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_parse_legacy_isa_reports_common_extensions() {
    let bits = parse_legacy_isa("rv64imafdc_zba_zbb_zicboz");

    assert_ne!(bits & IMA_EXT_FD, 0);
    assert_ne!(bits & IMA_EXT_C, 0);
    assert_ne!(bits & IMA_EXT_ZBA, 0);
    assert_ne!(bits & IMA_EXT_ZBB, 0);
    assert_ne!(bits & IMA_EXT_ZICBOZ, 0);
}

#[def_test]
fn test_parse_extension_list_maps_known_extensions() {
    // NUL-separated extension names, as found in `riscv,isa-extensions`.
    let bytes = b"zba\0zbb\0zicboz\0zba\0";
    let bits = parse_extension_list(bytes);

    assert_ne!(bits & IMA_EXT_ZBA, 0);
    assert_ne!(bits & IMA_EXT_ZBB, 0);
    assert_ne!(bits & IMA_EXT_ZICBOZ, 0);
    // Unknown extensions and duplicates must not corrupt the result.
    let unknown = parse_extension_list(b"zzz_unknown\0");
    assert_eq!(unknown, 0);
}

#[def_test]
fn test_hwprobe_key_classification_separates_bitmask_and_scalar() {
    // Bitmask keys, per Linux `hwprobe_key_is_bitmask`.
    for bitmask in [
        HwprobeKey::BaseBehavior,
        HwprobeKey::ImaExt0,
        HwprobeKey::ImaExt1,
        HwprobeKey::Cpuperf0,
        HwprobeKey::VendorExtThead0,
        HwprobeKey::VendorExtSifive0,
        HwprobeKey::VendorExtMips0,
    ] {
        assert!(bitmask.is_bitmask_aggregate());
    }

    for scalar in [
        HwprobeKey::Mvendorid,
        HwprobeKey::Marchid,
        HwprobeKey::Mimpid,
        HwprobeKey::ZicbozBlockSize,
        HwprobeKey::HighestVirtualAddress,
        HwprobeKey::TimeCsrFreq,
        HwprobeKey::MisalignedScalarPerf,
    ] {
        assert!(!scalar.is_bitmask_aggregate());
    }
}

#[def_test]
fn test_hwprobe_key_is_known_rejects_unknown_keys() {
    for key in 0..=RISCV_HWPROBE_KEY_IMA_EXT_1 {
        assert!(hwprobe_key_is_known(key));
    }
    assert!(!hwprobe_key_is_known(-1));
    assert!(!hwprobe_key_is_known(RISCV_HWPROBE_KEY_IMA_EXT_1 + 1));
}

#[def_test]
fn test_aggregate_pair_ands_bitmasks_and_detects_scalar_mismatch() {
    // Bitmask keys intersect across CPUs.
    assert_eq!(HwprobeKey::ImaExt0.aggregate_pair(0b1010, 0b0110), 0b0010);
    // Scalar keys keep the common value, else u64::MAX on mismatch.
    assert_eq!(HwprobeKey::Mvendorid.aggregate_pair(42, 42), 42);
    assert_eq!(HwprobeKey::Mvendorid.aggregate_pair(42, 7), u64::MAX);
}

#[def_test(serial)]
fn test_aggregate_and_match_against_snapshot() {
    // A single-CPU mask selects the conservative default snapshot values
    // (mvendorid = 0, base_behavior = IMA bit 0).
    let mut mask = KCpuMask::new();
    mask.set(0, true);
    let cpu0 = LogicalCpuId::new(0);

    // Scalar aggregation: MVENDORID is 0 on the default snapshot.
    assert_eq!(
        hwprobe_aggregate_value(RISCV_HWPROBE_KEY_MVENDORID, &mask),
        Some(0)
    );
    // Scalar matching: requested 0 matches, requested 1 does not.
    assert_eq!(
        hwprobe_cpu_matches(RISCV_HWPROBE_KEY_MVENDORID, cpu0, 0),
        Some(true)
    );
    assert_eq!(
        hwprobe_cpu_matches(RISCV_HWPROBE_KEY_MVENDORID, cpu0, 1),
        Some(false)
    );

    // Bitmask aggregation: BASE_BEHAVIOR is the IMA bit.
    assert_eq!(
        hwprobe_aggregate_value(RISCV_HWPROBE_KEY_BASE_BEHAVIOR, &mask),
        Some(BASE_BEHAVIOR_IMA)
    );
    // Bitmask matching: requesting the present bit matches; an absent bit
    // does not.
    assert_eq!(
        hwprobe_cpu_matches(RISCV_HWPROBE_KEY_BASE_BEHAVIOR, cpu0, BASE_BEHAVIOR_IMA),
        Some(true)
    );
    assert_eq!(
        hwprobe_cpu_matches(
            RISCV_HWPROBE_KEY_BASE_BEHAVIOR,
            cpu0,
            BASE_BEHAVIOR_IMA << 1
        ),
        Some(false)
    );
}

#[def_test(serial)]
fn test_aggregate_value_reports_unknown_keys_as_none() {
    let mask = KCpuMask::new();
    assert_eq!(hwprobe_aggregate_value(-1, &mask), None);
    assert_eq!(hwprobe_cpu_matches(999, LogicalCpuId::new(0), 0), None);
}
