// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI mechanism configuration helper delegating to the platform's `NmiMechanismIf`.

/// Configure NMI delivery for `hwirq` on the **current CPU**.
///
/// Delegates to
/// [`NmiMechanismIf::configure_nmi`](kplat::nm_irq::NmiMechanismIf::configure_nmi), so the
/// platform validates the mode and KIRQ owns local preparation admission.
/// Initial preparation requires a disabled registered CPU-local NMI source;
/// it must run on each participating CPU before enable. Repeated preparation
/// does not rewrite attributes, but still rejects a closing Line. Shared
/// sources are unsupported. This does not enable delivery or start a producer.
#[cfg(feature = "nmi")]
pub fn configure_nmi(hwirq: usize) -> bool {
    kplat::nm_irq::NmiMechanismIf::configure_nmi(hwirq)
}
