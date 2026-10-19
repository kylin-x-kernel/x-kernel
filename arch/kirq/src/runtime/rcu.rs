// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Static host bridge for interrupt watching, separate from locking IRQ hooks.

use krcu_core::RcuContextIf;

use crate::context::NmiContextGuard;

/// Runs NMI work with effective NMI context established before watching entry.
///
/// # Safety
/// The caller is on an initialized CPU with ordinary IRQs and migration
/// excluded. The closure must not schedule or abandon this stack. It must
/// finish all protected work and nested entries before returning.
pub(crate) unsafe fn with_nmi_context<R>(operation_fn: impl FnOnce() -> R) -> R {
    // SAFETY: the caller excludes ordinary IRQs and migration through the
    // closure; any nested NMI completes before this stack resumes.
    let nmi = unsafe { NmiContextGuard::enter() };
    // SAFETY: the same CPU exclusion covers the provider lookup and matching
    // exit. Effective NMI context is already visible to nested cleanup.
    let entry = unsafe { RcuContextIf::current_cpu_watching().enter_interrupt() };
    let result = operation_fn();
    // SAFETY: protected work and nested entries finished on this CPU. Keep NMI
    // context until watching has been restored, without any scheduling tail.
    unsafe { entry.exit() };
    drop(nmi);
    result
}

unittest::path_mod_test!(tests, "../../ut-tests/rcu_context.rs");
