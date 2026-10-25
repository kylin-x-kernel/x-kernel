// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use unittest::{assert, assert_eq, def_test};

// One-shot observations persist for this test boot; no runtime hooks are installed.
static HANDLERS_READY: AtomicBool = AtomicBool::new(false);
static PHASES: [AtomicU8; kbuild_config::NR_CPUS] = [const { AtomicU8::new(0) }; kbuild_config::NR_CPUS];
static HAS_INVALID_ORDER: AtomicBool = AtomicBool::new(false);

fn advance(expected: u8, next: u8) {
    let cpu = khal::percpu::this_cpu_id().as_usize();
    if karch::local_irq_enabled()
        || PHASES[cpu]
            .compare_exchange(expected, next, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    {
        HAS_INVALID_ORDER.store(true, Ordering::Release);
    }
}

pub(super) fn record_prepared() {
    advance(0, 1);
}

pub(super) fn record_handlers_ready() {
    HANDLERS_READY.store(true, Ordering::Release);
}

pub(super) fn record_activation_entry() {
    if !HANDLERS_READY.load(Ordering::Acquire) || ktask::current_may_uninit().is_none() {
        HAS_INVALID_ORDER.store(true, Ordering::Release);
    }
    advance(1, 2);
}

pub(super) fn record_activation_return() {
    advance(2, 3);
}

#[def_test]
fn local_sources_activate_after_preparation_and_handler_publication() {
    assert!(!HAS_INVALID_ORDER.load(Ordering::Acquire));
    assert!(HANDLERS_READY.load(Ordering::Acquire));
    #[cfg(feature = "smp")]
    let cpu_count = kcpu_id_map::nr_cpus();
    #[cfg(not(feature = "smp"))]
    let cpu_count = 1;
    for phase in &PHASES[..cpu_count] {
        assert_eq!(phase.load(Ordering::Acquire), 3);
    }
}
