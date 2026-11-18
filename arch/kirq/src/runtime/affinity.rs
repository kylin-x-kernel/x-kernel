// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lookup-only routing transactions and per-Line affinity state.
//!
//! Management admission retains identity; the short Line lock serializes chip
//! writes against enable, disable and flow tails. Immediate writes and deferred
//! ACK/EOI progress share this state; neither borrows actions, drains old claims
//! nor changes disable depth. Pending is accepted without a hardware write and
//! is advanced only from a real completion path.

use kcpu_id_map::KCpuMask;

use super::{
    manager::{ManagementMode, with_existing_line},
    state::{IRQ_STATE, IrqStateDesc},
};
use crate::{
    IrqAffinity, IrqAffinityApplyError, IrqAffinityDelivery, IrqAffinityError, IrqAffinitySnapshot,
    IrqDescError, IrqFlags, IrqFlow, Virq,
};

/// Reads confirmed runtime affinity without creating a mapping or accessing MMIO.
///
/// Rejects NMI before locking. Ordinary task/IRQ callers may query; the returned
/// copy is coherent for one retained Line but is not a mapping lease. PerCpu,
/// NMI, MSI and unsupported sources return Unsupported; fixed descriptor affinity
/// returns OwnerFixed. Before the first successful update, requested/effective
/// are None: firmware metadata is
/// not evidence of the actual route. Failed programming clears effective only.
pub fn irq_affinity(virq: Virq) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    if crate::context::is_in_nmi() {
        return Err(IrqAffinityError::Management(IrqDescError::InvalidContext {
            operation: "query IRQ affinity",
        }));
    }
    let line = { IRQ_STATE.lock().descs.get(&virq) }
        .ok_or(IrqAffinityError::Management(IrqDescError::UnknownIrq))?;
    let entry = line.lock();
    snapshot_locked(&entry)
}

/// Applies an ordinary source's routing policy in sleepable context.
///
/// The entire shared Line is routed, not one action. Only ready CPUs are offered
/// to the controller; the requested mask is retained separately from the
/// nonempty effective subset. Rejection preserves the previous policy. Success
/// does not wait for old deliveries or handlers and never changes disable depth.
///
/// Hardware failure invalidates effective routing and closes ordinary admission,
/// registration and enable. Recovery requires disable_irq_nosync unless already
/// logically disabled (including a never-enabled Line), then a successful routing
/// retry. Delivery resumes only through normal explicit enable/registration.
/// Neither failure nor software exclusion guarantees physical-source quiescence.
/// An immediate controller reports effective routing in the same call. A
/// deferred controller accepts the request with `is_pending`; only a later
/// edge ACK or level EOI can make it effective.
pub fn set_irq_affinity(
    virq: Virq,
    requested: &KCpuMask,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    with_existing_line(virq.into(), ManagementMode::Task, |_, line| {
        let mut entry = line.lock();
        let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
        Ok(apply_locked(&mut entry, requested, ready))
    })
    .map_err(IrqAffinityError::Management)?
    .ok_or(IrqAffinityError::Management(IrqDescError::UnknownIrq))?
}

enum AffinitySource {
    Immediate(&'static crate::IrqAffinityChip),
    Deferred(&'static crate::IrqDeferredAffinityChip),
}

fn check_source(entry: &IrqStateDesc) -> Result<AffinitySource, IrqAffinityError> {
    if entry.is_retired() {
        return Err(IrqAffinityError::Management(IrqDescError::UnknownIrq));
    }
    if entry.flow.is_none()
        || entry.flow == Some(IrqFlow::PerCpu)
        || entry.has_nmi_action()
        || entry.has_nmi_shutdown()
        || entry
            .desc
            .flags
            .intersects(IrqFlags::MSI | IrqFlags::PER_CPU)
    {
        return Err(IrqAffinityError::Unsupported);
    }
    if entry.desc.affinity != IrqAffinity::Any {
        return Err(IrqAffinityError::OwnerFixed);
    }
    if let Some(chip) = entry.affinity_chip() {
        return Ok(AffinitySource::Immediate(chip));
    }
    entry
        .deferred_affinity_chip()
        .map(AffinitySource::Deferred)
        .ok_or(IrqAffinityError::Unsupported)
}

/// Classifies the static source capability used to seed requested file modes.
///
/// This intentionally does not consult action presence, restrictions or the
/// affinity snapshot. The caller owns the Line lock.
pub(super) fn source_is_user_settable(entry: &IrqStateDesc) -> Result<bool, IrqAffinityError> {
    match check_source(entry) {
        Ok(_) => Ok(true),
        Err(IrqAffinityError::Unsupported | IrqAffinityError::OwnerFixed) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Copies affinity from an already locked Line state.
///
/// The caller owns the short Line lock. This helper never reacquires it and only
/// returns the historical coherent snapshot; it is not a mapping lease.
pub(super) fn snapshot_locked(
    entry: &IrqStateDesc,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    check_source(entry)?;
    Ok(entry.affinity)
}

/// Checks whether a user-originated routing change is currently admissible.
///
/// The caller holds the Line lock. Source/flow eligibility comes first, then
/// teardown/action presence, then owner policy. This is a sampled control-plane
/// result, not an authorization token: a later setter repeats all checks, and a
/// success does not promise any particular ready CPU or hardware target.
pub(super) fn check_user_affinity_change_locked(
    entry: &IrqStateDesc,
) -> Result<(), IrqAffinityError> {
    check_source(entry)?;
    if entry.is_teardown_in_progress() || entry.is_nmi_closing() {
        return Err(IrqAffinityError::Management(
            IrqDescError::TeardownInProgress {
                virq: entry.desc.logical_irq().unwrap_or(entry.desc.hwirq),
            },
        ));
    }
    if !entry.has_actions() {
        return Err(IrqAffinityError::Management(IrqDescError::NoIrqAction {
            virq: entry.desc.logical_irq().unwrap_or(entry.desc.hwirq),
        }));
    }
    if entry.has_user_affinity_restriction() {
        return Err(IrqAffinityError::UserRestricted);
    }
    Ok(())
}

/// Applies a user-originated routing change to an already locked Line state.
///
/// The caller must hold management admission and the Line lock. Policy denial
/// happens before the existing routing transaction and therefore before any
/// chip callback.
pub(super) fn apply_user_locked(
    entry: &mut IrqStateDesc,
    requested: &KCpuMask,
    ready: KCpuMask,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    check_user_affinity_change_locked(entry)?;
    apply_locked(entry, requested, ready)
}

/// Applies one affinity transaction to an already locked Line state.
///
/// The caller must hold management admission and the short Line lock. An Arc
/// identity alone is not permission to program hardware.
pub(super) fn apply_locked(
    entry: &mut IrqStateDesc,
    requested: &KCpuMask,
    ready: KCpuMask,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    let source = check_source(entry)?;
    if entry.is_teardown_in_progress() || entry.is_nmi_closing() {
        return Err(IrqAffinityError::Management(
            IrqDescError::TeardownInProgress {
                virq: entry.desc.logical_irq().unwrap_or(entry.desc.hwirq),
            },
        ));
    }
    let eligible = *requested & ready;
    if eligible.is_empty() {
        return Err(IrqAffinityError::NoReadyCpu);
    }
    if entry.affinity.has_failed_programming && !entry.is_affinity_disabled() {
        return Err(IrqAffinityError::RecoveryRequiresDisable);
    }
    let delivery = if entry.is_affinity_disabled() {
        IrqAffinityDelivery::KeepMasked
    } else {
        IrqAffinityDelivery::Preserve
    };
    match source {
        AffinitySource::Immediate(chip) => {
            let result = (chip.apply)(entry.desc, &eligible, delivery);
            match result {
                Ok(effective) if !effective.is_empty() && (effective & eligible) == effective => {
                    entry.affinity = IrqAffinitySnapshot {
                        requested: Some(*requested),
                        effective: Some(effective),
                        has_failed_programming: false,
                        is_pending: false,
                    };
                    Ok(entry.affinity)
                }
                Err(IrqAffinityApplyError::Rejected) => Err(IrqAffinityError::Rejected),
                // A malformed success also cannot certify routing. Keep the previous
                // accepted policy but never expose an unproven hardware destination.
                Ok(_) | Err(IrqAffinityApplyError::HardwareFailure) => {
                    entry.affinity.effective = None;
                    entry.affinity.has_failed_programming = true;
                    entry.affinity.is_pending = false;
                    Err(IrqAffinityError::HardwareFailure)
                }
            }
        }
        AffinitySource::Deferred(chip) => match (chip.accept)(entry.desc, &eligible) {
            Ok(()) => {
                entry.affinity.requested = Some(*requested);
                entry.affinity.is_pending = true;
                Ok(entry.affinity)
            }
            Err(IrqAffinityApplyError::Rejected) => Err(IrqAffinityError::Rejected),
            Err(IrqAffinityApplyError::HardwareFailure) => {
                entry.affinity.effective = None;
                entry.affinity.has_failed_programming = true;
                entry.affinity.is_pending = false;
                Err(IrqAffinityError::HardwareFailure)
            }
        },
    }
}

/// Runs deferred hardware progress while the source is actually suppressed.
///
/// This is used only from configuration/enable/recovery paths, where no real
/// EOI is guaranteed. It is gated by the current logical suppression state,
/// not by `effective == None`: a disabled Line with an already-confirmed route
/// still needs its pending target attempted. If the controller cannot finish
/// while Remote IRR remains set, a confirmed old route may resume service and
/// the request stays pending; without a confirmed route activation fails closed.
pub(super) fn apply_deferred_before_activation(
    entry: &mut IrqStateDesc,
) -> Result<(), IrqDescError> {
    if !entry.affinity.is_pending || !entry.is_affinity_disabled() {
        return Ok(());
    }
    let token = prepare_deferred_affinity(entry);
    if let Some(token) = token {
        finish_deferred_affinity(entry, token);
    }
    if entry.affinity.has_failed_programming {
        return Err(IrqDescError::ConfigurationFailed {
            hwirq: entry.desc.hwirq,
        });
    }
    if entry.affinity.is_pending && entry.affinity.effective.is_none() {
        return Err(IrqDescError::ConfigurationFailed {
            hwirq: entry.desc.hwirq,
        });
    }
    Ok(())
}

/// Runs the pre-EOI phase for an accepted deferred request.
///
/// Returns an opaque token only when the backend prepared a level transaction
/// that must be finished after EOI. Edge (`Complete`) commits effective here;
/// a hardware failure latches admission closed but never suppresses the
/// original EOI obligation.
pub(super) fn prepare_deferred_affinity(entry: &mut IrqStateDesc) -> Option<usize> {
    if !entry.affinity.is_pending {
        return None;
    }
    let chip = entry.deferred_affinity_chip()?;
    let requested = entry.affinity.requested?;
    let candidates = requested & kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    if candidates.is_empty() {
        return None;
    }
    let delivery = if entry.is_affinity_disabled() {
        IrqAffinityDelivery::KeepMasked
    } else {
        IrqAffinityDelivery::Preserve
    };
    match (chip.prepare)(entry.desc, &candidates, delivery) {
        Ok(crate::IrqAffinityPrepare::Complete(effective))
            if !effective.is_empty() && (effective & candidates) == effective =>
        {
            entry.affinity.effective = Some(effective);
            entry.affinity.has_failed_programming = false;
            entry.affinity.is_pending = false;
            None
        }
        Ok(crate::IrqAffinityPrepare::Pending(token)) => Some(token),
        Ok(crate::IrqAffinityPrepare::Complete(_)) => {
            entry.affinity.effective = None;
            entry.affinity.has_failed_programming = true;
            entry.affinity.is_pending = false;
            None
        }
        Err(IrqAffinityApplyError::Rejected) => None,
        Err(IrqAffinityApplyError::HardwareFailure) => {
            entry.affinity.effective = None;
            entry.affinity.has_failed_programming = true;
            entry.affinity.is_pending = false;
            None
        }
    }
}

/// Runs the post-EOI phase for a prepared level transaction.
pub(super) fn finish_deferred_affinity(entry: &mut IrqStateDesc, token: usize) {
    let Some(chip) = entry.deferred_affinity_chip() else {
        return;
    };
    let Some(requested) = entry.affinity.requested else {
        return;
    };
    let candidates = requested & kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    if candidates.is_empty() {
        return;
    }
    let delivery = if entry.is_affinity_disabled() {
        IrqAffinityDelivery::KeepMasked
    } else {
        IrqAffinityDelivery::Preserve
    };
    match (chip.finish)(entry.desc, token, delivery) {
        Ok(Some(effective)) if !effective.is_empty() && (effective & candidates) == effective => {
            entry.affinity.effective = Some(effective);
            entry.affinity.has_failed_programming = false;
            entry.affinity.is_pending = false;
        }
        Ok(Some(_)) => {
            entry.affinity.effective = None;
            entry.affinity.has_failed_programming = true;
            entry.affinity.is_pending = false;
        }
        Ok(None) => {}
        Err(IrqAffinityApplyError::Rejected) => {}
        Err(IrqAffinityApplyError::HardwareFailure) => {
            entry.affinity.effective = None;
            entry.affinity.has_failed_programming = true;
            entry.affinity.is_pending = false;
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/affinity.rs");
