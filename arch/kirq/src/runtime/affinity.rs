// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Lookup-only routing transactions and confirmed per-Line affinity state.
//!
//! Management admission retains identity; the short Line lock serializes chip
//! writes against enable, disable and flow tails. No routing operation borrows
//! actions, drains old claims or changes disable depth. Deferred controller
//! migration must supply real hardware progress before joining this interface.

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
    check_source(&entry)?;
    Ok(entry.affinity)
}

/// Immediately applies an ordinary source's routing policy in sleepable context.
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
/// This immediate-only entry rejects sources requiring deferred migration.
pub fn set_irq_affinity(
    virq: Virq,
    requested: &KCpuMask,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    with_existing_line(virq.into(), ManagementMode::Task, |_, line| {
        let mut entry = line.lock();
        let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
        Ok(apply(&mut entry, requested, ready))
    })
    .map_err(IrqAffinityError::Management)?
    .ok_or(IrqAffinityError::Management(IrqDescError::UnknownIrq))?
}

fn check_source(entry: &IrqStateDesc) -> Result<&'static crate::IrqAffinityChip, IrqAffinityError> {
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
    entry.affinity_chip().ok_or(IrqAffinityError::Unsupported)
}

fn apply(
    entry: &mut IrqStateDesc,
    requested: &KCpuMask,
    ready: KCpuMask,
) -> Result<IrqAffinitySnapshot, IrqAffinityError> {
    let chip = check_source(entry)?;
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
    let result = (chip.apply)(entry.desc, &eligible, delivery);
    match result {
        Ok(effective) if !effective.is_empty() && (effective & eligible) == effective => {
            entry.affinity = IrqAffinitySnapshot {
                requested: Some(*requested),
                effective: Some(effective),
                has_failed_programming: false,
            };
            Ok(entry.affinity)
        }
        Err(IrqAffinityApplyError::Rejected) => Err(IrqAffinityError::Rejected),
        // A malformed success also cannot certify routing. Keep the previous
        // accepted policy but never expose an unproven hardware destination.
        Ok(_) | Err(IrqAffinityApplyError::HardwareFailure) => {
            entry.affinity.effective = None;
            entry.affinity.has_failed_programming = true;
            Err(IrqAffinityError::HardwareFailure)
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/affinity.rs");
