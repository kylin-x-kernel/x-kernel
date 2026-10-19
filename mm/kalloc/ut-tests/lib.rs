// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use strum::VariantArray;
use unittest::def_test;

use super::{UsageCounters, UsageKind, UsageTotals};

fn snapshot(counters: &[&UsageCounters]) -> super::Usages {
    let mut totals = UsageTotals::new();
    for counter in counters {
        totals.collect_freed(counter);
    }
    for counter in counters {
        totals.collect_allocated(counter);
    }
    totals.finish()
}

#[def_test]
fn test_usages_alloc_dealloc() {
    let usages = UsageCounters::new();
    usages.alloc(UsageKind::RustHeap, 64);
    assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 64);
    usages.dealloc(UsageKind::RustHeap, 32);
    assert_eq!(snapshot(&[&usages]).get(UsageKind::RustHeap), 32);
}

#[def_test]
fn test_usage_kind_variants_len() {
    assert!(UsageKind::VARIANTS.len() >= 5);
}

#[def_test]
fn test_usages_independent_kinds() {
    let usages = UsageCounters::new();
    usages.alloc(UsageKind::VirtMem, 10);
    usages.alloc(UsageKind::PageTable, 20);
    let snapshot = snapshot(&[&usages]);
    assert_eq!(snapshot.get(UsageKind::VirtMem), 10);
    assert_eq!(snapshot.get(UsageKind::PageTable), 20);
}

#[def_test]
fn test_usages_allow_cross_cpu_free() {
    let allocating_cpu = UsageCounters::new();
    let freeing_cpu = UsageCounters::new();
    allocating_cpu.alloc(UsageKind::RustHeap, 64);
    freeing_cpu.dealloc(UsageKind::RustHeap, 64);

    assert_eq!(
        snapshot(&[&allocating_cpu, &freeing_cpu]).get(UsageKind::RustHeap),
        0
    );
}
