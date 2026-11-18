// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec::Vec};
use unittest::{assert, assert_eq, def_test};

use crate::{
    KCpuMask, KCpuMaskExt,
    text::{
        CpuTextDomain, CpuTextError, bitmap_capacity, list_capacity, parse_bitmap, parse_list,
        write_bitmap, write_list,
    },
};

fn mask(cpus: &[usize]) -> KCpuMask {
    let mut mask = KCpuMask::new();
    for cpu in cpus {
        mask.set_logical(crate::LogicalCpuId::new(*cpu), true);
    }
    mask
}

fn domain(cpus: &[usize]) -> CpuTextDomain {
    CpuTextDomain::new(mask(cpus)).unwrap()
}

fn bits(mask: KCpuMask) -> Vec<usize> {
    mask.iter_logical()
        .map(crate::LogicalCpuId::as_usize)
        .collect()
}

fn has_width(required: usize) -> bool {
    kbuild_config::NR_CPUS >= required
}

fn write<F>(f: F) -> Result<String, CpuTextError>
where
    F: FnOnce(&mut String) -> Result<(), CpuTextError>,
{
    let mut out = String::new();
    f(&mut out)?;
    Ok(out)
}

#[def_test]
fn bitmap_linux_boundary_vectors() {
    if !has_width(5) {
        return unittest::TestResult::Ignored;
    }
    let width4 = domain(&[0, 1, 2, 3]);
    let width5 = domain(&[0, 1, 2, 3, 4]);

    assert_eq!(bits(parse_bitmap(b"f", &width4).unwrap()), [0, 1, 2, 3]);
    assert_eq!(
        parse_bitmap(b"10", &width4),
        Err(CpuTextError::Overflow)
    );
    assert_eq!(
        bits(parse_bitmap(b"1f", &width5).unwrap()),
        [0, 1, 2, 3, 4]
    );
    assert_eq!(
        parse_bitmap(b"20", &width5),
        Err(CpuTextError::Overflow)
    );
}

#[def_test]
fn bitmap_width33_boundary_vector() {
    if !has_width(33) {
        return unittest::TestResult::Ignored;
    }
    let width33 = domain(&[0, 32]);
    assert_eq!(
        parse_bitmap(b"2,00000000", &width33),
        Err(CpuTextError::Overflow)
    );
}

#[def_test]
fn bitmap_width64_boundary_vectors() {
    if !has_width(64) {
        return unittest::TestResult::Ignored;
    }
    let domain = domain(&[0, 63]);
    assert_eq!(
        bits(parse_bitmap(b"00000000,00000001", &domain).unwrap()),
        [0]
    );
    assert_eq!(
        bits(parse_bitmap(b"80000000,00000000", &domain).unwrap()),
        [63]
    );
    assert_eq!(
        write(|out| write_bitmap(mask(&[0, 63]), &domain, out)).unwrap(),
        "80000000,00000001"
    );
    assert!(bitmap_capacity(&domain).unwrap() >= 17);
    assert_eq!(bits(parse_list(b"0,63", &domain).unwrap()), [0, 63]);
}

#[def_test]
fn bitmap_accepts_linux_empty_groups_and_short_suffix() {
    if !has_width(4) {
        return unittest::TestResult::Ignored;
    }
    let domain = domain(&[0, 1, 2, 3]);
    assert!(parse_bitmap(b"", &domain).unwrap().is_empty());
    assert!(parse_bitmap(b",,", &domain).unwrap().is_empty());
    assert_eq!(bits(parse_bitmap(b"f", &domain).unwrap()), [0, 1, 2, 3]);
    assert_eq!(bits(parse_bitmap(b"1\n0", &domain).unwrap()), [0]);
    assert_eq!(
        parse_bitmap(b"1,00000000", &domain),
        Err(CpuTextError::Overflow)
    );
    assert_eq!(
        parse_bitmap(b"0,00000000", &domain),
        Err(CpuTextError::Overflow)
    );
}

#[def_test]
fn bitmap_checks_present_membership_after_width() {
    if !has_width(4) {
        return unittest::TestResult::Ignored;
    }
    let sparse = domain(&[0, 3]);
    assert_eq!(bits(parse_bitmap(b"9", &sparse).unwrap()), [0, 3]);
    assert_eq!(
        parse_bitmap(b"f", &sparse),
        Err(CpuTextError::OutOfRange)
    );
    assert_eq!(
        parse_bitmap(b"12", &sparse),
        Err(CpuTextError::Overflow)
    );
}

#[def_test]
fn list_linux_range_and_sparse_present_vectors() {
    if !has_width(4) {
        return unittest::TestResult::Ignored;
    }
    let full = domain(&[0, 1, 2, 3]);
    let sparse = domain(&[0, 3]);

    assert!(parse_list(b"", &full).unwrap().is_empty());
    assert_eq!(bits(parse_list(b"0 3", &sparse).unwrap()), [0, 3]);
    assert_eq!(bits(parse_list(b"0,3", &sparse).unwrap()), [0, 3]);
    assert_eq!(bits(parse_list(b"N", &sparse).unwrap()), [3]);
    assert_eq!(bits(parse_list(b"0-3", &full).unwrap()), [0, 1, 2, 3]);
    assert_eq!(bits(parse_list(b"ALL", &full).unwrap()), [0, 1, 2, 3]);
    assert_eq!(bits(parse_list(b"all:1/2", &full).unwrap()), [0, 2]);
    assert_eq!(bits(parse_list(b"0-3:1/2", &full).unwrap()), [0, 2]);
    assert_eq!(bits(parse_list(b"0-3:2/2", &full).unwrap()), [0, 1, 2, 3]);
    assert_eq!(bits(parse_list(b"0-3:N/N", &full).unwrap()), [0, 1, 2, 3]);
    assert_eq!(bits(parse_list(b"0\n1", &full).unwrap()), [0]);

    assert_eq!(
        parse_list(b"all", &sparse),
        Err(CpuTextError::OutOfRange)
    );
    assert_eq!(
        parse_list(b"0-N", &sparse),
        Err(CpuTextError::OutOfRange)
    );
    assert_eq!(
        parse_list(b"0 - 3", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0- 3", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0-3 :2/4", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0-3: 2/4", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"3-0", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0-3:2/0", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0-3:3/2", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0-4", &full),
        Err(CpuTextError::OutOfRange)
    );
    assert_eq!(
        parse_list(b"999999999999999999999", &full),
        Err(CpuTextError::Overflow)
    );
    assert_eq!(
        parse_list(b"allx", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(
        parse_list(b"0:1/1", &full),
        Err(CpuTextError::InvalidSyntax)
    );
    assert_eq!(bits(parse_list(b"0\x0b3", &sparse).unwrap()), [0, 3]);
    assert_eq!(bits(parse_list(b"0\x0c3", &sparse).unwrap()), [0, 3]);
    assert_eq!(
        parse_list(b"1,x", &sparse),
        Err(CpuTextError::InvalidSyntax)
    );
}

#[def_test]
fn single_cpu_vectors_run_in_the_current_up_config() {
    let one = domain(&[0]);
    assert_eq!(bits(parse_bitmap(b"1", &one).unwrap()), [0]);
    assert_eq!(bits(parse_bitmap(b"", &one).unwrap()), Vec::<usize>::new());
    assert_eq!(
        parse_bitmap(b"f", &one),
        Err(CpuTextError::Overflow)
    );
    assert_eq!(bits(parse_list(b"0", &one).unwrap()), [0]);
    assert_eq!(bits(parse_list(b"N", &one).unwrap()), [0]);
    assert_eq!(bits(parse_list(b"all", &one).unwrap()), [0]);
    assert!(parse_list(b"0-1", &one).is_err());
    assert_eq!(
        write(|out| write_bitmap(mask(&[0]), &one, out)).unwrap(),
        "1"
    );
    assert_eq!(
        write(|out| write_list(mask(&[0]), &one, out)).unwrap(),
        "0"
    );
}

#[def_test]
fn formatter_uses_runtime_width_and_no_newline() {
    if !has_width(8) {
        return unittest::TestResult::Ignored;
    }
    let width4 = domain(&[0, 1, 2, 3]);
    let width5 = domain(&[0, 1, 2, 3, 4]);
    let width8 = domain(&[0, 1, 2, 3, 4, 5, 6, 7]);
    let value = mask(&[0, 2]);

    assert_eq!(
        write(|out| write_bitmap(value, &width4, out)).unwrap(),
        "5"
    );
    assert_eq!(
        write(|out| write_bitmap(value, &width5, out)).unwrap(),
        "05"
    );
    assert_eq!(
        write(|out| write_list(value, &width4, out)).unwrap(),
        "0,2"
    );
    assert_eq!(
        write(|out| write_bitmap(KCpuMask::new(), &width4, out)).unwrap(),
        "0"
    );
    assert_eq!(
        write(|out| write_bitmap(KCpuMask::new(), &width8, out)).unwrap(),
        "00"
    );
    assert_eq!(
        write(|out| write_list(KCpuMask::new(), &width4, out)).unwrap(),
        ""
    );

    let text = write(|out| write_bitmap(mask(&[0, 3]), &width8, out)).unwrap();
    assert!(bitmap_capacity(&width8).unwrap() >= text.len());
    let text = write(|out| write_list(mask(&[0, 2, 4, 6]), &width8, out)).unwrap();
    assert!(list_capacity(&width8).unwrap() >= text.len());
}

#[def_test]
fn domain_rejects_empty_and_derives_width_from_highest_present_id() {
    assert_eq!(
        CpuTextDomain::new(KCpuMask::new()).err(),
        Some(CpuTextError::EmptyDomain)
    );
    if !has_width(4) {
        return unittest::TestResult::Ignored;
    }
    let sparse = domain(&[0, 3]);
    assert_eq!(sparse.nr_cpu_ids(), 4);
    assert_eq!(sparse.present().iter_logical().count(), 2);
    assert!(crate::nr_cpu_ids() >= 1);
    assert!(crate::nr_cpu_ids() <= kbuild_config::NR_CPUS);
}
