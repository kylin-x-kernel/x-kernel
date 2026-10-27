// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI registration and dispatch state.

use alloc::collections::BTreeMap;

use kspin::{SpinRaw, static_lock};

use super::manager::{
    IRQ_CONTROL_LOCK, ManagementMode, with_existing_line, with_registration_line_checked,
};
use crate::{
    Hwirq, IrqDesc, IrqFlags, IrqSpec,
    action::IrqAction,
    platform::{Handler, configure_platform_irq, set_platform_irq_enabled},
    state::IRQ_STATE,
};

static_lock! {
    /// NMI handler table, keyed by hwirq.
    ///
    /// # Locking invariant
    ///
    /// - **WRITES**: source-quiesced management via [`register_nmi`],
    ///   [`register_nmi_startup`] and [`unregister_nmi`], serialized with the
    ///   fallback action by the manager control lock after Line admission.
    ///   Management from NMI is rejected before any lock.
    /// - **READS**: NMI context only, via [`dispatch_nmi_handler`]. The lock is never
    ///   acquired from a normal IRQ handler, so an NMI that preempts a
    ///   normal IRQ never contends on this lock.
    ///
    /// [`SpinRaw`] (no IRQ / preempt guards) is therefore safe:
    ///
    /// - boot-time writers run before any NMI can be delivered; later writers
    ///   must exclude all local NMI readers of this shared table, not just the
    ///   source being changed, and quiesce that source before removal. This
    ///   exclusion must survive management waits and migration, covering every
    ///   CPU on which the managing task can resume;
    /// - writers acquire the sleepable management mutex before this table lock.
    ///   While holding the table lock they also hold control/registry/Line
    ///   SpinNoIrq guards: they cannot sleep, be preempted or migrate. Local
    ///   NMI exclusion is still required because those guards do not mask NMI;
    /// - a pseudo-NMI cannot preempt another pseudo-NMI on the same CPU (the NMI
    ///   path never lowers PMR below `NMI_ONLY`);
    /// - a hardware NMI can preempt a normal IRQ handler (the IRQ path opens the
    ///   ALLINT window), but IRQ handlers never touch this table, and the NMI
    ///   path keeps `PSTATE.ALLINT` set so a hardware NMI cannot preempt another
    ///   NMI.
    static NMI_TABLE: SpinRaw<BTreeMap<Hwirq, Handler>> = SpinRaw::new(BTreeMap::new());
}

/// Register an NMI handler for a hardware interrupt.
///
/// The interrupt is configured as an NMI by the platform (GIC priority 0 for
/// pseudo-NMI, the GICv3.3 NMI attribute for hardware NMI) and routed through
/// the separate `dispatch_nmi_handler` path. Unlike
/// `register`, this function **never** acquires `IRQ_STATE.lock()` during
/// dispatch — the handler is stored in a separate `NMI_TABLE` keyed by hwirq.
///
/// # Safety constraints
///
/// - NMI handlers must be **per-CPU** (enforced by tagging the descriptor with
///   `IrqFlags::PER_CPU`).
/// - NMI handlers **cannot be shared** — duplicate registration on the same
///   hwirq is rejected.
/// - Refuses to overwrite a regular handler already registered on the same
///   line, and rejects duplicates before touching any internal state.
/// - Requires sleepable task context and may wait for Line management exclusion.
///   Use [`register_nmi_startup`] for explicit nonwaiting startup registration.
/// - The caller must quiesce the affected source and exclude all local NMI
///   readers of the shared table throughout this call, including after sleeping
///   or migration to another CPU. Merely masking NMI on the entry CPU is not
///   sufficient. The exclusion must not make ordinary task waiting unsafe.
///   This requires globally stopped NMI delivery or a caller-owned NMI
///   exclusion mechanism valid across migration; task context alone does not
///   satisfy it. This API supplies neither mechanism. The table lock itself
///   is held only under short IRQ/preemption exclusion, never across a wait.
///   This API does not establish NMI quiescence or drain NMI handlers.
/// - NMI-context calls are rejected before taking any lock.
pub fn register_nmi(desc: IrqDesc, handler: Handler) -> bool {
    register_nmi_with_mode(desc, handler, ManagementMode::Task)
}

/// Registers an NMI handler during explicit, allocator-ready startup.
///
/// Requires an installed current task and no interrupt/BH context. IRQs may
/// still be disabled. Never waits for Line management exclusion; contention
/// returns false rather than falling back to ordinary task acquisition.
/// All [`register_nmi`]'s source/table quiescence obligations apply. Complete
/// startup registration before allowing NMI delivery; this is not a runtime
/// atomic-context registration API.
pub fn register_nmi_startup(desc: IrqDesc, handler: Handler) -> bool {
    register_nmi_with_mode(desc, handler, ManagementMode::Startup)
}

fn register_nmi_with_mode(desc: IrqDesc, handler: Handler, mode: ManagementMode) -> bool {
    let hwirq = desc.hwirq;
    let result = with_registration_line_checked(
        IrqSpec::Desc(desc.with_flags(IrqFlags::PER_CPU)),
        mode,
        |line| {
            // The helper already holds control/state exclusion. Reject before
            // descriptor resolution can merge flags or publish a new mapping.
            let has_table_entry = { NMI_TABLE.lock().contains_key(&hwirq) };
            Ok(!has_table_entry && !line.lock().has_actions())
        },
        |_, line| {
            // Registration keeps control exclusion from preflight through
            // publication, including across different Lines sharing a hwirq.
            let state = IRQ_STATE.lock();
            let mut entry = line.lock();
            let mut table = NMI_TABLE.lock();
            debug_assert!(!table.contains_key(&hwirq) && !entry.has_actions());
            if !entry.install_regular_action(IrqAction::nmi_fallback(handler.clone())) {
                return Ok(false);
            }
            let stored_desc = entry.desc;
            table.insert(hwirq, handler.clone());
            drop(table);
            drop(entry);
            drop(state);
            configure_platform_irq(stored_desc);
            Ok(true)
        },
    );
    // Even rejected installation retains the client's last owner until after
    // the helper releases management exclusion and all of its short locks.
    drop(handler);
    match result {
        Ok(Some(true)) => true,
        rejected => {
            // NMI rejection must not enter the potentially locking logger.
            // The management helper has released all locks before diagnostics.
            if !crate::context::is_in_nmi() {
                match rejected {
                    Err(err) => warn!("NMI registration failed: hwirq={hwirq}, error={err:?}"),
                    _ => warn!(
                        "NMI registration rejected: hwirq={hwirq} (occupied, teardown or \
                         management contention)"
                    ),
                }
            }
            false
        }
    }
}

/// Remove a previously registered NMI handler.
///
/// Besides removing the `NMI_TABLE` entry, this clears the fallback handler
/// and `IrqFlags::PER_CPU` tag that [`register_nmi`] stored in `IRQ_STATE`,
/// so a re-enabled or re-triggered IRQ no longer dispatches the removed
/// handler through the normal path. The platform line is disabled when it is
/// no longer used, using the full stored descriptor.
/// Requires sleepable task context. The caller must first quiesce the source
/// and exclude all local NMI readers of the shared table throughout the call,
/// including management waits and every CPU on which it may resume after
/// migration. Entry-CPU masking alone does not suffice. This API does not drain
/// NMI handlers; the existing caller-owned quiescence obligation is unchanged.
/// Globally stopped NMI delivery or a caller-owned exclusion mechanism valid
/// across migration is required; ordinary task context alone is insufficient.
/// This API supplies neither mechanism. It acquires the sleepable management
/// mutex before locking the table and never sleeps while holding the table lock.
/// Calls from NMI context are rejected before taking any lock.
pub fn unregister_nmi(desc: IrqDesc) -> bool {
    let result = with_existing_line(IrqSpec::Desc(desc), ManagementMode::Task, |_, line| {
        let _control_guard = IRQ_CONTROL_LOCK.lock();
        let state = IRQ_STATE.lock();
        let mut entry = line.lock();
        let mut table = NMI_TABLE.lock();
        if entry.desc.hwirq != desc.hwirq || !table.contains_key(&desc.hwirq) {
            return Ok(None);
        }
        let Some(removed_fallback) = entry.take_nmi_fallback() else {
            return Ok(None);
        };
        // Keep legacy force-enable excluded until admission cleanup atomically
        // finishes teardown and retires this original Line after masking.
        entry.begin_teardown();
        let removed_handler = table.remove(&desc.hwirq);
        entry.remove_flags(IrqFlags::PER_CPU);
        let stored_desc = entry.desc;
        drop(table);
        drop(entry);
        drop(state);
        // NMI setup does not track platform enable state in the normal IRQ
        // runtime. Disable explicitly, even if old fallback dispatch is live.
        set_platform_irq_enabled(stored_desc, false);
        Ok(Some((removed_fallback, removed_handler)))
    });
    let was_removed = matches!(&result, Ok(Some(_)));
    // The helper cleans up only the original Line and releases its mutex
    // before these owners can run reentrant client destructors.
    drop(result);
    was_removed
}

/// Dispatch a registered NMI handler without touching [`IRQ_STATE`].
///
/// The handler is cloned out of [`NMI_TABLE`] before the lock is released,
/// but management from the handler is still forbidden: it would acquire normal
/// IRQ locks which the NMI may have interrupted.
pub(super) fn dispatch_nmi_handler(hwirq: Hwirq) {
    let handler = {
        let table = NMI_TABLE.lock();
        table.get(&hwirq).cloned()
    };
    if let Some(handler) = handler {
        let _ = handler.handle(hwirq);
    } else {
        warn!("Unhandled NMI for hwirq {hwirq}");
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/nmi_management.rs");
