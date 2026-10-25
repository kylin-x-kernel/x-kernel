// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use riscv::register::sie;
use unittest::{assert, assert_eq, def_test};

#[def_test(serial)]
fn source_activation_preserves_global_irq_mask() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let saved = sie::read();
    let mut masked = saved;
    masked.set_ssoft(false);
    masked.set_stimer(false);
    masked.set_sext(false);
    // SAFETY: Global IRQ delivery and migration are excluded; only this hart's
    // source enables change, and the saved state is restored before assertions.
    unsafe { sie::write(masked) };

    super::activate_local_interrupts();
    let activated = sie::read();
    let is_irq_enabled = karch::local_irq_enabled();

    // SAFETY: The guard still excludes delivery and migration on the same hart.
    unsafe { sie::write(saved) };

    let mut expected = saved;
    expected.set_ssoft(true);
    expected.set_stimer(true);
    expected.set_sext(true);
    assert_eq!(activated.bits(), expected.bits());
    assert!(!is_irq_enabled);
}
