// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stop-thread callback runs on the caller's CPU before `stop_one_cpu` returns.

use core::sync::atomic::{AtomicBool, Ordering};

use khal::percpu::this_cpu_id;
use unittest::{assert, def_test};

use crate::stop_one_cpu;

static STOP_HIT: AtomicBool = AtomicBool::new(false);

fn mark_stop(_arg: *mut ()) {
    STOP_HIT.store(true, Ordering::Release);
}

#[def_test]
fn stop_one_cpu_runs_callback_before_return() {
    STOP_HIT.store(false, Ordering::Release);
    assert!(stop_one_cpu(
        this_cpu_id(),
        mark_stop,
        core::ptr::null_mut()
    ));
    assert!(STOP_HIT.load(Ordering::Acquire));
}
