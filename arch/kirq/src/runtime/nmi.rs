// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pseudo-NMI registration and dispatch state.

use alloc::collections::BTreeMap;

use kspin::SpinRaw;

use crate::{
    Hwirq, IrqDesc, IrqFlags, IrqSpec,
    action::IrqAction,
    platform::{Handler, configure_and_enable_platform_irq, disable_platform_irq},
    state::{IRQ_STATE, IrqStateDesc, try_resolve_and_publish},
};

/// NMI handler table, keyed by hwirq.
///
/// # Locking invariant
///
/// - **WRITES**: boot-time registration via [`register_nmi`] /
///   [`unregister_nmi`], or — rarely — from an NMI handler itself (see
///   [`dispatch_nmi_handler`]). The normal IRQ path and process context never write
///   this table.
/// - **READS**: NMI context only, via [`dispatch_nmi_handler`]. The lock is never
///   acquired from a normal IRQ handler, so a pseudo-NMI that preempts a
///   normal IRQ never contends on this lock.
///
/// [`SpinRaw`] (no IRQ / preempt guards) is therefore safe: boot-time writers
/// run before any NMI can be delivered, and a pseudo-NMI cannot preempt
/// another pseudo-NMI on the same CPU, so a writer and a reader never run
/// concurrently on the same CPU.
static NMI_TABLE: SpinRaw<BTreeMap<Hwirq, Handler>> = SpinRaw::new(BTreeMap::new());

/// Register an NMI handler for a hardware interrupt.
///
/// The interrupt is configured as a pseudo-NMI with the highest GIC priority
/// and routed through the lock-free `dispatch_nmi_handler` path. Unlike
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
/// - Normally called **at boot time**, before `enable_local_irq()`, so that
///   no NMI can fire before registration is complete. It may also be called
///   from an NMI handler itself — a pseudo-NMI cannot preempt another
///   pseudo-NMI on the same CPU, so the registration cannot race a reader.
pub fn register_nmi(desc: IrqDesc, handler: Handler) -> bool {
    let hwirq = desc.hwirq;

    // Reject duplicate NMI registrations before touching any state, so a
    // failed registration cannot leave IRQ_STATE or NMI_TABLE inconsistent.
    if NMI_TABLE.lock().contains_key(&hwirq) {
        warn!("register_nmi: handler already exists for hwirq {hwirq}");
        return false;
    }

    // Resolve descriptor in IRQ_STATE (metadata tracking + fallback handler
    // for when nmi-pmu is not enabled and dispatch goes through the normal path).
    let mut state = IRQ_STATE.lock();
    // Refuse to overwrite an existing regular action on this line, mirroring
    // register()'s duplicate regular-action check.
    if let Some(virq) = state.lookup_virq(IrqSpec::Desc(desc))
        && state
            .descs
            .get(&virq)
            .is_some_and(IrqStateDesc::has_regular_action)
    {
        warn!("register_nmi: handler already registered for irq {virq}");
        return false;
    }
    let desc = match try_resolve_and_publish(
        &mut state,
        IrqSpec::Desc(desc.with_flags(IrqFlags::PER_CPU)),
    ) {
        Ok(desc) => desc,
        Err(err) => {
            warn!("register_nmi: incompatible descriptor for hwirq {hwirq}: {err:?}");
            return false;
        }
    };
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after try_resolve_desc");
    // Tag the remembered descriptor so descriptor() queries see the NMI flag.
    entry.update_desc(desc);
    // Store a fallback handler for the non-NMI dispatch path.
    let installed = entry.install_regular_action(IrqAction::regular(handler.clone()));
    debug_assert!(installed, "regular action was checked above");
    drop(state);

    // Store handler in NMI table (keyed by hwirq).
    NMI_TABLE.lock().insert(hwirq, handler);

    // Pass the resolved descriptor so platform binding/configuration also
    // applies when the line only carries a dynamically allocated virq.
    configure_and_enable_platform_irq(desc, true);
    true
}

/// Remove a previously registered NMI handler.
///
/// Besides removing the `NMI_TABLE` entry, this clears the fallback handler
/// and `IrqFlags::PER_CPU` tag that [`register_nmi`] stored in `IRQ_STATE`,
/// so a re-enabled or re-triggered IRQ no longer dispatches the removed
/// handler through the normal path. The platform line is disabled when it is
/// no longer used, using the full stored descriptor.
pub fn unregister_nmi(desc: IrqDesc) -> bool {
    let hwirq = desc.hwirq;
    let removed = {
        let mut table = NMI_TABLE.lock();
        table.remove(&hwirq).is_some()
    };
    if !removed {
        return false;
    }

    // Also clear the IRQ_STATE fallback handler and PER_CPU tag installed by
    // register_nmi, so a re-enabled or re-triggered IRQ no longer dispatches
    // the removed handler through the normal path.
    let mut state = IRQ_STATE.lock();
    let Some(virq) = state.lookup_virq(IrqSpec::Desc(desc)) else {
        return true;
    };
    if let Some(entry) = state.descs.get_mut(&virq)
        && entry.has_regular_action()
    {
        entry.take_regular_action();
        entry.remove_flags(IrqFlags::PER_CPU);
    }
    let disabled_desc = state.remove_if_unused_with_desc(virq);
    drop(state);
    if let Some(stored_desc) = disabled_desc {
        // Carry the full stored descriptor so the disable is not silently
        // skipped for lines whose hwirq falls above DYNAMIC_VIRQ_BASE.
        disable_platform_irq(stored_desc);
    }
    true
}

/// Dispatch a registered NMI handler without touching [`IRQ_STATE`].
///
/// The handler is cloned out of [`NMI_TABLE`] before the lock is released,
/// so the handler itself may safely (but rarely) call `register_nmi` /
/// `unregister_nmi` without self-deadlock.
pub(super) fn dispatch_nmi_handler(hwirq: Hwirq) {
    let handler = {
        let table = NMI_TABLE.lock();
        table.get(&hwirq).cloned()
    };
    if let Some(handler) = handler {
        let _ = handler.handle();
    } else {
        warn!("Unhandled NMI for hwirq {hwirq}");
    }
}
