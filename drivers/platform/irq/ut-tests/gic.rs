// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

#[def_test]
fn repeated_local_initialization_is_rejected_before_hardware_setup() {
    // Kernel tests run after this CPU's mandatory controller preparation.
    // Keep the current CPU and mask stable while testing the rejection path.
    let _guard = kspin::NoPreemptIrqSave::new();
    #[cfg(feature = "nmi-pseudo")]
    let saved_pmr = karch::pmr::is_ready().then(karch::pmr::read);

    let result = super::init_current_cpu();
    assert_eq!(result, Err("local GIC initialization already attempted"));
    assert_eq!(karch::local_irq_enabled(), false);
    #[cfg(feature = "nmi-pseudo")]
    if let Some(saved) = saved_pmr {
        assert_eq!(karch::pmr::read(), saved);
    }
}
