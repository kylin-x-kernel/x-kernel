// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static REGISTER_BARRIER_ENABLED: AtomicBool = AtomicBool::new(false);
static REGISTER_BARRIER_ARRIVALS: AtomicUsize = AtomicUsize::new(0);
static REGISTER_BARRIER_RELEASED: AtomicBool = AtomicBool::new(false);

pub(super) fn arm_register_barrier() {
    REGISTER_BARRIER_ARRIVALS.store(0, Ordering::Release);
    REGISTER_BARRIER_RELEASED.store(false, Ordering::Release);
    REGISTER_BARRIER_ENABLED.store(true, Ordering::Release);
}

pub(super) fn register_barrier_arrivals() -> usize {
    REGISTER_BARRIER_ARRIVALS.load(Ordering::Acquire)
}

pub(super) fn release_register_barrier() {
    REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
}

pub(super) fn disarm_register_barrier() {
    REGISTER_BARRIER_ENABLED.store(false, Ordering::Release);
    REGISTER_BARRIER_RELEASED.store(true, Ordering::Release);
}

pub(super) fn pause_after_register_if_armed() {
    if !REGISTER_BARRIER_ENABLED.load(Ordering::Acquire) {
        return;
    }

    REGISTER_BARRIER_ARRIVALS.fetch_add(1, Ordering::AcqRel);
    while !REGISTER_BARRIER_RELEASED.load(Ordering::Acquire) {
        ktask::yield_now();
    }
}
