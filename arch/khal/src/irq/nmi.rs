// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI mechanism configuration helper delegating to the platform's `NmiDef`.

/// Configure NMI delivery for `hwirq` on the **current CPU**.
///
/// Delegates to
/// [`NmiDef::configure_nmi`](kplat::nm_irq::NmiDef::configure_nmi), so the
/// platform owns the mode‑specific controller setup.  For per‑CPU interrupt
/// lines (PPIs) this must be called on every CPU; for shared lines (SPIs)
/// the write is idempotent across CPUs.
#[cfg(feature = "nmi")]
pub fn configure_nmi(hwirq: usize) -> bool {
    kplat::nm_irq::NmiDef::configure_nmi(hwirq)
}
