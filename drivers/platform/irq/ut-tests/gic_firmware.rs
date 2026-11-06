// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;
use unittest::{assert_eq, def_test};

struct Dtb {
    structure: Vec<u8>,
    strings: Vec<u8>,
}

impl Dtb {
    fn new() -> Self {
        Self {
            structure: Vec::new(),
            strings: Vec::new(),
        }
    }

    fn word(&mut self, value: u32) {
        self.structure.extend_from_slice(&value.to_be_bytes());
    }

    fn align(&mut self) {
        while !self.structure.len().is_multiple_of(4) {
            self.structure.push(0);
        }
    }

    fn begin(&mut self, name: &str) {
        self.word(1);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.align();
        self.word(4);
    }

    fn property(&mut self, name: &str, bytes: &[u8]) {
        self.word(3);
        self.word(bytes.len() as u32);
        self.word(self.strings.len() as u32);
        self.strings.extend_from_slice(name.as_bytes());
        self.strings.push(0);
        self.structure.extend_from_slice(bytes);
        self.align();
    }

    fn end(&mut self) {
        self.word(2);
    }

    fn finish(mut self) -> Vec<u8> {
        self.word(9);
        let strings_offset = 56 + self.structure.len();
        let mut bytes = Vec::new();
        for value in [
            0xd00d_feed,
            (strings_offset + self.strings.len()) as u32,
            56,
            strings_offset as u32,
            40,
            17,
            16,
            0,
            self.strings.len() as u32,
            self.structure.len() as u32,
        ] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes.extend_from_slice(&[0; 16]);
        bytes.extend_from_slice(&self.structure);
        bytes.extend_from_slice(&self.strings);
        bytes
    }
}

fn cells(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|value| value.to_be_bytes()).collect()
}

fn config(bytes: &[u8]) -> Result<Option<super::GicConfig>, super::GicConfigError> {
    let tree = rs_fdtree::LinuxFdt::new(bytes).unwrap();
    let backend = rs_fdtree::DtBackend::new(tree);
    super::GicConfig::from_fwnode(backend.root().unwrap())
}

fn fixture(status: Option<&[u8]>, ranges: Option<&[u32]>, reg: &[u32]) -> Vec<u8> {
    fixture_with_compatible(b"arm,gic-400\0", status, ranges, reg)
}

fn fixture_with_compatible(
    compatible: &[u8],
    status: Option<&[u8]>,
    ranges: Option<&[u32]>,
    reg: &[u32],
) -> Vec<u8> {
    fixture_with_properties(compatible, status, ranges, reg, &[])
}

fn fixture_with_properties(
    compatible: &[u8],
    status: Option<&[u8]>,
    ranges: Option<&[u32]>,
    reg: &[u32],
    extra: &[(&str, &[u8])],
) -> Vec<u8> {
    let mut blob = Dtb::new();
    blob.begin("");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    blob.begin("bus");
    blob.property("#address-cells", &cells(&[1]));
    blob.property("#size-cells", &cells(&[1]));
    if let Some(ranges) = ranges {
        blob.property("ranges", &cells(ranges));
    }
    blob.begin("interrupt-controller");
    blob.property("compatible", compatible);
    blob.property("interrupt-controller", &[]);
    if let Some(status) = status {
        blob.property("status", status);
    }
    blob.property("reg", &cells(reg));
    for (name, value) in extra {
        blob.property(name, value);
    }
    blob.end();
    blob.end();
    blob.end();
    blob.finish()
}

#[def_test]
fn discovery_skips_disabled_controller() {
    let bytes = fixture(
        Some(b"disabled\0"),
        Some(&[]),
        &[0x1000, 0x1000, 0x2000, 0x1000],
    );
    assert_eq!(config(&bytes), Ok(None));
}

#[def_test]
fn discovery_translates_complete_register_regions() {
    let bytes = fixture(None, Some(&[0, 0x8000000, 0x10000]), &[0x1000, 0x1000, 0x2000, 0x1000]);
    let gic = config(&bytes).unwrap().unwrap();
    assert_eq!(gic.gicd.paddr.as_usize(), 0x8001000);
    assert_eq!(gic.gicc.unwrap().paddr.as_usize(), 0x8002000);
}

#[def_test]
fn discovery_rejects_malformed_status_instead_of_skipping_it() {
    let bytes = fixture(Some(b"okay"), Some(&[]), &[0x1000, 0x1000, 0x2000, 0x1000]);
    unittest::assert!(matches!(
        config(&bytes),
        Err(super::GicConfigError::Status { .. })
    ));
}

#[def_test]
fn discovery_requires_both_distributor_and_cpu_interface() {
    for reg in [&[][..], &[0x1000, 0x1000][..]] {
        let bytes = fixture(None, Some(&[]), reg);
        unittest::assert!(matches!(
            config(&bytes),
            Err(super::GicConfigError::MissingRegion { .. })
        ));
    }
}

#[def_test]
fn discovery_rejects_missing_or_incomplete_address_translation() {
    for ranges in [None, Some(&[0, 0x8000000, 0x1800][..]), Some(&[0][..])] {
        let bytes = fixture(None, ranges, &[0x1000, 0x1000, 0x2000, 0x1000]);
        unittest::assert!(matches!(
            config(&bytes),
            Err(super::GicConfigError::Registers { .. })
        ));
    }
}

#[def_test]
fn discovery_validates_trailing_and_zero_sized_regions() {
    for reg in [
        &[0x1000, 0x1000, 0x2000, 0x1000, 7][..],
        &[0x1000, 0, 0x2000, 0x1000][..],
    ] {
        let bytes = fixture(None, Some(&[]), reg);
        unittest::assert!(matches!(
            config(&bytes),
            Err(super::GicConfigError::Registers { .. })
        ));
    }
}

#[def_test]
fn discovery_preserves_gicv2_optional_regions_and_available_statuses() {
    for status in [None, Some(&b"ok\0"[..]), Some(&b"okay\0"[..])] {
        let bytes = fixture(
            status,
            Some(&[]),
            &[0x1000, 0x1000, 0x2000, 0x1000, 0x3000, 0x1000, 0x4000, 0x1000],
        );
        let gic = config(&bytes).unwrap().unwrap();
        assert_eq!(gic.version, super::GicVersion::V2);
        assert_eq!(gic.gich.unwrap().paddr.as_usize(), 0x3000);
        assert_eq!(gic.gicv.unwrap().paddr.as_usize(), 0x4000);
        assert!(gic.gicr.is_none());
    }
}

#[def_test]
fn discovery_preserves_gicv3_first_redistributor_selection() {
    let bytes = fixture_with_compatible(
        b"arm,gic-v3\0",
        None,
        Some(&[]),
        &[0x1000, 0x1000, 0x2000, 0x20000, 0x40000, 0x20000],
    );
    let gic = config(&bytes).unwrap().unwrap();
    assert_eq!(gic.version, super::GicVersion::V3);
    let gicr = gic.gicr.unwrap();
    assert_eq!(gicr.count, 1);
    assert_eq!(gicr.regions().next().unwrap().paddr.as_usize(), 0x2000);
    assert!(gic.gicc.is_none());
    assert!(gic.gich.is_none());
}

#[def_test]
fn discovery_preserves_multiple_redistributor_regions_and_stride() {
    let bytes = fixture_with_properties(
        b"arm,gic-v3\0",
        None,
        Some(&[]),
        &[0x1000, 0x1000, 0x2000, 0x20000, 0x22000, 0x20000],
        &[
            ("#redistributor-regions", &cells(&[2])),
            ("redistributor-stride", &cells(&[0x20000])),
        ],
    );
    let gic = config(&bytes).unwrap().unwrap();
    let gicr = gic.gicr.unwrap();
    assert_eq!(gicr.count, 2);
    assert_eq!(gicr.stride, 0x20000);
    let regions: Vec<_> = gicr.regions().collect();
    assert_eq!(regions.len(), 2);
    assert_eq!(regions[0].paddr.as_usize(), 0x2000);
    assert_eq!(regions[0].size, 0x20000);
    assert_eq!(regions[1].paddr.as_usize(), 0x22000);
    assert_eq!(regions[1].size, 0x20000);
}

#[def_test]
fn discovery_rejects_invalid_redistributor_region_counts() {
    for count in [0, super::MAX_GICR_REGIONS + 1] {
        let count = count as u32;
        let bytes = fixture_with_properties(
            b"arm,gic-v3\0",
            None,
            Some(&[]),
            &[0x1000, 0x1000, 0x2000, 0x20000],
            &[("#redistributor-regions", &cells(&[count]))],
        );
        unittest::assert!(matches!(
            config(&bytes),
            Err(super::GicConfigError::InvalidRedistributorCount { .. }
                | super::GicConfigError::TooManyRegions { .. })
        ));
    }
}

#[def_test]
fn discovery_requires_each_declared_redistributor_region() {
    let bytes = fixture_with_properties(
        b"arm,gic-v3\0",
        None,
        Some(&[]),
        &[0x1000, 0x1000, 0x2000, 0x20000],
        &[("#redistributor-regions", &cells(&[2]))],
    );
    unittest::assert!(matches!(
        config(&bytes),
        Err(super::GicConfigError::MissingRegion { .. })
    ));
}

#[def_test]
fn discovery_does_not_fall_back_after_an_invalid_available_candidate() {
    for first_status in [b"okay\0".as_slice(), b"disabled\0".as_slice()] {
        let mut blob = Dtb::new();
        blob.begin("");
        blob.property("#address-cells", &cells(&[1]));
        blob.property("#size-cells", &cells(&[1]));
        for (name, status, reg) in [
            ("first", first_status, &[0x1000, 0x1000][..]),
            (
                "second",
                b"okay\0".as_slice(),
                &[0x3000, 0x1000, 0x4000, 0x1000][..],
            ),
        ] {
            blob.begin(name);
            blob.property("compatible", b"arm,gic-400\0");
            blob.property("interrupt-controller", &[]);
            blob.property("status", status);
            blob.property("reg", &cells(reg));
            blob.end();
        }
        blob.end();
        let bytes = blob.finish();
        let result = config(&bytes);
        if first_status == b"okay\0" {
            unittest::assert!(matches!(
                result,
                Err(super::GicConfigError::MissingRegion { .. })
            ));
        } else {
            assert_eq!(result.unwrap().unwrap().gicd.paddr.as_usize(), 0x3000);
        }
    }
}
