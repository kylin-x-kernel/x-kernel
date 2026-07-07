// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime per-class lock-stat registration for `Mutex::new` / `RwLock::new`.
//!
//! `#[track_caller]` on those constructors identifies the call site. All lock
//! instances created from the same call site share one lock class.

use alloc::{boxed::Box, collections::BTreeMap, format, vec::Vec};
use core::{
    cell::UnsafeCell,
    panic::Location,
    sync::atomic::{AtomicBool, Ordering},
};

use crate::LockClassStats;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct LocationKey {
    file: &'static str,
    line: u32,
    column: u32,
}

struct Registry {
    by_class: BTreeMap<LocationKey, &'static LockClassStats>,
    runtime_stats: Vec<&'static LockClassStats>,
}

struct RegistryLock {
    locked: AtomicBool,
    registry: UnsafeCell<Registry>,
}

impl RegistryLock {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            registry: UnsafeCell::new(Registry {
                by_class: BTreeMap::new(),
                runtime_stats: Vec::new(),
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Registry) -> R) -> R {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }

        // SAFETY: the spin bit serializes exclusive access to `registry`.
        let result = unsafe { f(&mut *self.registry.get()) };

        self.locked.store(false, Ordering::Release);
        result
    }
}

// SAFETY: `RegistryLock` serializes access through the atomic `locked` flag.
unsafe impl Sync for RegistryLock {}

static REGISTRY: RegistryLock = RegistryLock::new();

/// Returns the lock class for `loc`, registering it on first use.
pub fn class_for_init_site(loc: &'static Location, kind: &'static str) -> &'static LockClassStats {
    let key = LocationKey {
        file: loc.file(),
        line: loc.line(),
        column: loc.column(),
    };

    REGISTRY.with(|registry| {
        if let Some(&stats) = registry.by_class.get(&key) {
            return stats;
        }

        let location = leak_class_label(loc);
        let stats = Box::leak(Box::new(LockClassStats::new(location, kind)));
        registry.by_class.insert(key, stats);
        registry.runtime_stats.push(stats);
        stats
    })
}

/// Returns snapshots for runtime-registered lock statistics.
pub fn runtime_stats_snapshot() -> Vec<crate::LockClassEntry> {
    REGISTRY.with(|registry| {
        registry
            .runtime_stats
            .iter()
            .map(|stats| stats.snapshot())
            .collect()
    })
}

fn leak_class_label(loc: &'static Location) -> &'static str {
    let location = format!("{}:{}", loc.file(), loc.line());
    Box::leak(location.into_boxed_str())
}

#[cfg(all(test, feature = "stats"))]
mod tests {
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
}
