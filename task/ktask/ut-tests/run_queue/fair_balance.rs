// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pure balance policy tests, independent of IRQ and SMP timing.

use ksched::{FairBalanceStats, PeltAvg};
use unittest::{assert, assert_eq, def_test};

use super::{KCpuMask, LoadHint, find_source_cpu, is_busier, migration_improves_balance};

fn stats(nr_running: usize, load_avg: u64) -> FairBalanceStats {
    FairBalanceStats {
        nr_running,
        nr_queued: nr_running.saturating_sub(1),
        is_idle: nr_running == 0,
        pelt: PeltAvg {
            load_avg,
            runnable_avg: nr_running as u64 * 1024,
            util_avg: 1024,
        },
    }
}

#[def_test]
fn equally_busy_cpus_balance_three_tasks_against_one() {
    let source = stats(3, 3072);
    let destination = stats(1, 1024);
    assert!(migration_improves_balance(source, destination, stats(1, 1024).pelt));
    assert!(!migration_improves_balance(stats(2, 2048), stats(2, 2048), stats(1, 1024).pelt));
    assert!(!migration_improves_balance(stats(3, 3072), stats(2, 2048), stats(1, 1024).pelt));
}

#[def_test]
fn balance_preserves_small_nice_weights_and_rejects_overshoot() {
    assert!(migration_improves_balance(stats(3, 9), stats(1, 3), stats(1, 3).pelt));
    assert!(!migration_improves_balance(stats(2, 4096), stats(1, 1024), stats(1, 4096).pelt));
    assert!(!migration_improves_balance(stats(1, 4096), stats(1, 1024), stats(1, 4096).pelt));
    assert!(!migration_improves_balance(stats(2, 1024), stats(1, 1000), stats(1, 8).pelt));
}

#[def_test]
fn empty_destination_ignores_retained_blocked_history() {
    let destination = stats(0, 8192);
    assert!(migration_improves_balance(stats(2, 2048), destination, stats(1, 1024).pelt));
    assert!(is_busier(LoadHint { load_avg: 2048, nr_queued: 1 }, destination));
    assert!(!is_busier(LoadHint { load_avg: 8192, nr_queued: 0 }, stats(1, 1024)));
    // With RT running, zero Fair tasks is not an idle CPU. It does not
    // receive the unconditional idle-fill exception to the load comparison.
    assert!(!migration_improves_balance(
        stats(2, 2048),
        FairBalanceStats { is_idle: false, ..destination },
        stats(1, 1024).pelt,
    ));
}

#[def_test]
fn sole_fair_waiter_behind_rt_can_fill_an_idle_cpu() {
    let source = FairBalanceStats { nr_queued: 1, ..stats(1, 1024) };
    assert!(is_busier(LoadHint { load_avg: 1024, nr_queued: 1 }, stats(0, 8192)));
    assert!(migration_improves_balance(source, stats(0, 8192), stats(1, 1024).pelt));
}

#[def_test]
fn rotated_source_search_reaches_lighter_donors_after_pinned_sources() {
    if kbuild_config::NR_CPUS < 4 {
        return unittest::TestResult::Ignored;
    }
    let destination = stats(1, 1024);
    let mut tried = KCpuMask::new();
    let load_of = |cpu| {
        Some(LoadHint {
            load_avg: [8192, 4096, 1024, 3072][cpu],
            nr_queued: usize::from(cpu != 2),
        })
    };
    assert_eq!(find_source_cpu(2, destination, &tried, 0..4, load_of), Some(0));
    // The previous source cannot donate because of affinity. A later pass
    // resumes beyond it, even though it remains the heaviest source.
    tried.set(0, true);
    assert_eq!(find_source_cpu(2, destination, &tried, 1..4, load_of), Some(1));
    assert_eq!(find_source_cpu(2, destination, &KCpuMask::new(), 2..4, load_of), Some(3));
}
