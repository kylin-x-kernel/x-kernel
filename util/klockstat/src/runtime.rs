// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free runtime lock-class registration.

use alloc::vec::Vec;
use core::{
    panic::Location,
    sync::atomic::{AtomicBool, Ordering},
};

use klazy::Once;

use crate::{LockClassStats, LockLocation, NOOP_CLASS};

const MAX_RUNTIME_CLASSES: usize = 1024;

struct RuntimeClass {
    location: &'static Location<'static>,
    stats: LockClassStats,
}

struct Registry<const CAPACITY: usize> {
    slots: [Once<RuntimeClass>; CAPACITY],
    is_enabled: AtomicBool,
}

impl<const CAPACITY: usize> Registry<CAPACITY> {
    const fn new() -> Self {
        Self {
            slots: [const { Once::new() }; CAPACITY],
            is_enabled: AtomicBool::new(true),
        }
    }

    fn class_for(
        &self,
        location: &'static Location<'static>,
        kind: &'static str,
    ) -> &LockClassStats {
        if !self.is_enabled.load(Ordering::Relaxed) {
            return &NOOP_CLASS;
        }
        let hash = location.file().bytes().chain(kind.bytes()).fold(
            location.line() as usize ^ location.column() as usize,
            |hash, byte| hash.wrapping_mul(31).wrapping_add(byte as usize),
        );
        for offset in 0..CAPACITY {
            let slot = &self.slots[(hash % CAPACITY + offset) % CAPACITY];
            let entry = slot.call_once(|| RuntimeClass {
                location,
                stats: LockClassStats::with_location(LockLocation::Caller(location), kind),
            });
            if entry.location == location && entry.stats.kind() == kind {
                return &entry.stats;
            }
        }
        self.is_enabled.store(false, Ordering::Relaxed);
        &NOOP_CLASS
    }

    fn snapshots(&self) -> Vec<crate::LockClassEntry> {
        self.slots
            .iter()
            .filter_map(|slot| slot.get().map(|entry| entry.stats.snapshot()))
            .collect()
    }
}

static REGISTRY: Registry<MAX_RUNTIME_CLASSES> = Registry::new();

#[cfg(target_os = "none")]
struct IrqRestore(usize);

#[cfg(target_os = "none")]
impl Drop for IrqRestore {
    fn drop(&mut self) {
        karch::restore_irq(self.0);
    }
}

/// Returns a stable class without allocating or sleeping.
///
/// Task and ordinary IRQ contexts are supported; NMI registration is not.
/// Exhausting the fixed class pool permanently disables all statistics until
/// reboot and returns the untracked class. Existing references stay valid.
pub fn class_for_init_site(
    location: &'static Location<'static>,
    kind: &'static str,
) -> &'static LockClassStats {
    #[cfg(target_os = "none")]
    let _irq_restore = IrqRestore(karch::save_irq_and_disable());
    REGISTRY.class_for(location, kind)
}

pub(crate) fn is_enabled() -> bool {
    REGISTRY.is_enabled.load(Ordering::Relaxed)
}

pub(crate) fn runtime_stats_snapshot() -> Vec<crate::LockClassEntry> {
    REGISTRY.snapshots()
}

#[cfg(all(test, feature = "stats"))]
#[path = "../tests/host/runtime.rs"]
mod tests;
