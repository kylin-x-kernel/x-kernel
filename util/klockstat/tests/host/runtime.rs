// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::panic::Location;

use super::*;

#[test]
fn class_for_init_site_reuses_same_class() {
    fn lookup() -> &'static LockClassStats {
        class_for_init_site(Location::caller(), "Mutex")
    }

    let first = lookup();
    let second = lookup();
    assert!(core::ptr::eq(first, second));
    assert_eq!(first.location(), second.location());
}

#[test]
fn exhaustion_disables_registry_without_invalidating_classes() {
    let registry = Registry::<2>::new();
    let location = Location::caller();
    let first = registry.class_for(location, "Mutex");
    let second = registry.class_for(location, "RwLock");
    assert!(!core::ptr::eq(first, second));
    assert!(core::ptr::eq(first, registry.class_for(location, "Mutex")));
    assert!(registry.is_enabled.load(Ordering::Relaxed));
    assert!(core::ptr::eq(
        &NOOP_CLASS,
        registry.class_for(location, "Other")
    ));
    assert!(!registry.is_enabled.load(Ordering::Relaxed));
    assert!(core::ptr::eq(
        &NOOP_CLASS,
        registry.class_for(location, "Mutex")
    ));
    assert_eq!(first.kind(), "Mutex");
    assert_eq!(registry.snapshots().len(), 2);
}

#[test]
fn concurrent_registration_publishes_one_class() {
    let registry = Registry::<8>::new();
    let location = Location::caller();
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    let stats = registry.class_for(location, "Mutex");
                    stats.record_acquisitions(1);
                    assert_eq!(registry.snapshots().len(), 1);
                    stats as *const LockClassStats as usize
                })
            })
            .collect();
        let addresses: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert!(addresses.iter().all(|address| *address == addresses[0]));
    });
    let snapshots = registry.snapshots();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].acquisitions, 8);
}

#[test]
fn runtime_label_preserves_file_and_line() {
    let registry = Registry::<1>::new();
    let location = Location::caller();
    let stats = registry.class_for(location, "Mutex");
    assert_eq!(
        stats.location().to_string(),
        format!("{}:{}", location.file(), location.line())
    );
}

#[test]
fn disabled_statistics_stop_existing_static_and_runtime_counters() {
    const CHILD_ENV: &str = "KLOCKSTAT_DISABLED_TEST_CHILD";
    if std::env::var_os(CHILD_ENV).is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime::tests::disabled_statistics_stop_existing_static_and_runtime_counters",
            ])
            .env(CHILD_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }
    let static_stats = LockClassStats::new("static", "Mutex");
    let runtime_stats = class_for_init_site(Location::caller(), "Mutex");
    static_stats.record(2, 3);
    runtime_stats.record(4, 5);
    let before_static = static_stats.snapshot();
    let before_runtime = runtime_stats.snapshot();
    REGISTRY.is_enabled.store(false, Ordering::Relaxed);
    assert!(!crate::statistics_enabled());
    static_stats.record(6, 7);
    runtime_stats.record(8, 9);
    assert_eq!(static_stats.snapshot(), before_static);
    assert_eq!(runtime_stats.snapshot(), before_runtime);
    assert!(crate::dump_lock_stat().starts_with("lock statistics disabled:"));
}
