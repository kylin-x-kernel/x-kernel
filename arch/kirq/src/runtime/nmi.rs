// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI registration and dispatch state.
//!
//! Management publishes a Line-owned handler into prepared CPU-local slots.
//! Dispatch only borrows that publication under directory RCU; shutdown revokes
//! local access before recovering the handler in task context. Controller
//! preparation is distinct from stopping the event producer, which remains the
//! source owner's responsibility.

use kalloc_types::KBox;
use kspin::NoPreemptIrqSave;

mod action;
mod shutdown;
pub(super) use action::NmiAction;
pub use shutdown::{NmiShutdown, begin_nmi_shutdown};

use super::manager::{
    ManagementMode, validate_management, with_action_removal, with_existing_line,
    with_registration_line_checked,
};
use crate::{
    IrqDesc, IrqDescError, IrqFlags, IrqRef, IrqSpec,
    platform::{Handler, IntrManagerIf, configure_platform_irq, set_platform_irq_enabled},
    state::IRQ_STATE,
};

/// Prepares the executing CPU's disabled NMI source without enabling delivery.
///
/// The platform must have initialized this CPU's NMI mechanism. Ordinary
/// task/IRQ context is accepted; NMI context is rejected. The operation pins
/// the CPU and holds the retained Line lock through bounded controller setup.
/// Failed setup retains a teardown obligation and prohibits local enable.
///
/// # Errors
/// Rejects absent/non-NMI actions, unsupported flows, closing Lines, initial
/// preparation of enabled sources, and controller failures. A previously
/// successful preparation is idempotent. It neither allocates nor waits for
/// another CPU. Retry or local teardown is required after partial failure.
pub fn prepare_percpu_nmi(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    with_local_nmi(spec.into(), |virq, line, entry| {
        if entry.is_nmi_closing() || entry.is_teardown_in_progress() {
            return Err(IrqDescError::TeardownInProgress { virq });
        }
        let chip = IntrManagerIf::percpu_nmi(entry.desc).ok_or(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        })?;
        prepare_local_action(line, entry, chip)
    })
}

fn prepare_local_action(
    line: &crate::state::IrqLine,
    entry: &mut crate::state::IrqStateDesc,
    chip: &'static crate::PerCpuNmiChip,
) -> Result<(), IrqDescError> {
    entry.prepare_local_nmi(chip)?;
    // SAFETY: the caller retains this Line's lock and CPU/IRQ exclusion outside
    // NMI. The sole action is installed and preparation has succeeded.
    if unsafe { line.nmi_action.publish_current() } {
        Ok(())
    } else {
        Err(IrqDescError::ConfigurationFailed {
            hwirq: entry.desc.hwirq,
        })
    }
}

/// Removes NMI attributes from the executing CPU's disabled source.
///
/// Allowed during Closing. Failure preserves the preparation obligation and
/// blocks local enable; only successful teardown releases that CPU's record.
///
/// # Safety
/// The source owner must stop local event generation before calling and keep it
/// stopped through teardown and subsequent removal/reuse. No outstanding claim
/// may cross the transition. This operation can clear hardware pending state;
/// it does not stop the producer or synchronize remote CPUs/handlers.
///
/// # Errors
/// Rejects NMI context, absent/non-NMI actions, unsupported flows, enabled local
/// sources and controller failures. A failure does not authorize owner release.
pub unsafe fn teardown_percpu_nmi(spec: impl Into<IrqSpec>) -> Result<(), IrqDescError> {
    with_local_nmi(spec.into(), |_, line, entry| {
        entry.teardown_local_nmi()?;
        // SAFETY: the caller excludes outstanding claims and producer restart.
        // Local teardown completed outside NMI under continuous Line/CPU/IRQ
        // exclusion. No finalizer can observe the cleared record before revoke.
        unsafe { line.nmi_action.clear_current() };
        Ok(())
    })
}

fn with_local_nmi(
    spec: IrqSpec,
    operation: impl FnOnce(
        usize,
        &crate::state::IrqLine,
        &mut crate::state::IrqStateDesc,
    ) -> Result<(), IrqDescError>,
) -> Result<(), IrqDescError> {
    if crate::context::is_in_nmi() {
        return Err(IrqDescError::InvalidContext {
            operation: "local NMI control",
        });
    }
    let _context = NoPreemptIrqSave::new();
    let state = IRQ_STATE.lock();
    let virq = state.lookup_virq(spec).ok_or(IrqDescError::UnknownIrq)?;
    let line = state.descs.get(&virq).ok_or(IrqDescError::UnknownIrq)?;
    let mut entry = line.lock();
    drop(state);
    if entry.is_retired() || !entry.has_nmi_action() {
        return Err(IrqDescError::NoIrqAction { virq });
    }
    if entry.flow != Some(crate::IrqFlow::PerCpu) {
        return Err(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        });
    }
    operation(virq, &line, &mut entry)
}

/// Registers the sole Line-owned handler for NMI-class delivery.
///
/// Requires a previously mapped descriptor and sleepable task context. The
/// source owner must keep delivery stopped until registration and CPU-local
/// preparation are complete. This tags the Line PER_CPU, but neither starts the
/// producer nor enables a CPU's source. Duplicate/regular/shared ownership is
/// rejected. Only PerCpu flows are supported. Ordinary IRQ dispatch rejects
/// this action and only completes its claim; the irqchip must select the NMI
/// entry for NMI-class delivery, including IRQ-on entry. Hardware
/// configuration failure publishes no handler; partially
/// written hardware remains the source owner's cleanup responsibility.
///
/// Dispatch uses a typed Domain lookup and CPU-local action publication.
/// Directory RCU protects the Line; local teardown ends action access.
/// No exclusion of unrelated NMI sources is required. Destruction runs in task context.
pub fn register_nmi(desc: IrqDesc, handler: Handler) -> bool {
    register_nmi_with_mode(desc, handler, ManagementMode::Task, configure_platform_irq)
}

/// Registers an NMI handler during allocator-ready startup without waiting.
///
/// Requires an installed task outside IRQ/BH/NMI context. IRQs may be masked;
/// management contention fails instead of sleeping. Mapping and source shutdown
/// prerequisites are the same as for [`register_nmi`]. Publication needs no
/// running RCU service. Action removal likewise does not wait for a grace period.
pub fn register_nmi_startup(desc: IrqDesc, handler: Handler) -> bool {
    register_nmi_with_mode(
        desc,
        handler,
        ManagementMode::Startup,
        configure_platform_irq,
    )
}

fn register_nmi_with_mode(
    desc: IrqDesc,
    handler: Handler,
    mode: ManagementMode,
    configure: impl FnOnce(IrqDesc) -> Result<(), IrqDescError>,
) -> bool {
    register_nmi_with_flow(desc, handler, mode, core::convert::identity, configure)
}

// Keep synthetic controller selection at this private boundary; production
// always preserves the flow resolved by the real irqchip before configuration.
fn register_nmi_with_flow(
    desc: IrqDesc,
    handler: Handler,
    mode: ManagementMode,
    select_flow: impl FnOnce(crate::IrqFlow) -> crate::IrqFlow,
    configure: impl FnOnce(IrqDesc) -> Result<(), IrqDescError>,
) -> bool {
    let result = validate_management(mode).and_then(|()| {
        // Keep the offered allocation outside the helper so rejection cannot
        // finally destroy a client's handler under management or short locks.
        let mut prepared = Some(KBox::new_nowait(handler).map_err(|_| IrqDescError::NoMemory)?);
        with_registration_line_checked(
            IrqSpec::Desc(desc.with_flags(IrqFlags::PER_CPU)),
            mode,
            |entry| Ok(entry.can_install_nmi_action()),
            |_, line, entry| {
                let flow = select_flow(entry.flow.ok_or(IrqDescError::UnsupportedFlow {
                    hwirq: entry.desc.hwirq,
                })?);
                if flow != crate::IrqFlow::PerCpu {
                    return Err(IrqDescError::UnsupportedFlow {
                        hwirq: entry.desc.hwirq,
                    });
                }
                entry.flow = Some(flow);
                let initial_policy = entry.prepare_initial_affinity_policy()?;
                if let Err(error) = configure(entry.desc) {
                    entry.mark_configuration_failed();
                    return Err(error);
                }
                let configured = entry.prepare_reconfigure_if_stale();
                entry.commit_platform_plan(configured);
                let Some(owner) = prepared.take() else {
                    return Ok(false);
                };
                if let Err(owner) = line.nmi_action.try_publish(owner) {
                    prepared = Some(owner);
                    return Ok(false);
                }
                entry.install_nmi_action();
                entry.commit_initial_affinity_policy(initial_policy);
                Ok(true)
            },
        )
    });
    match result {
        Ok(Some(true)) => true,
        rejected => {
            if !crate::context::is_in_nmi() {
                warn!("NMI registration rejected: desc={desc:?}, result={rejected:?}");
            }
            false
        }
    }
}

/// Removes a Line's NMI action after CPU-local shutdown and publication revocation.
///
/// Requires sleepable task context. The source
/// owner must stop event generation, serialize against restart, and complete
/// disable plus [`teardown_percpu_nmi`] on every prepared CPU. Enqueuing remote
/// work is not completion. Pending/active hardware claims must not cross reuse.
///
/// Removal closes enable/preparation admission. An enabled or prepared slot
/// returns false, retaining ownership and Closing so local cleanup can finish.
/// Local teardown revokes access on its CPU only after original claim completion.
/// Once all slots are clear, withdrawal recovers the owner without an action
/// grace period. Destruction runs after management unlock.
/// The descriptor and its mappings survive. Unrelated NMI sources may run
/// throughout; no global NMI exclusion or NMI table lock is required.
pub fn unregister_nmi(desc: IrqDesc) -> bool {
    unregister_nmi_with(desc, |stored| set_platform_irq_enabled(stored, false))
}

fn unregister_nmi_with(desc: IrqDesc, mask: impl FnOnce(IrqDesc)) -> bool {
    unregister_nmi_retained(desc, None, mask)
}

fn unregister_nmi_retained(
    desc: IrqDesc,
    retained: Option<&alloc::sync::Arc<crate::state::IrqLine>>,
    mask: impl FnOnce(IrqDesc),
) -> bool {
    let result = with_action_removal(IrqSpec::Desc(desc), ManagementMode::Task, |_, line| {
        if retained.is_some_and(|original| !alloc::sync::Arc::ptr_eq(original, line)) {
            return Ok(None);
        }
        let mut entry = line.lock();
        if entry.desc.hwirq != desc.hwirq || !entry.has_nmi_action() {
            return Ok(None);
        }
        if entry.has_nmi_shutdown() != retained.is_some() {
            return Ok(None);
        }
        entry.begin_nmi_close();
        if entry.has_enabled_local_cpu() || entry.has_local_nmi_preparation() {
            return Ok(None);
        }
        // SAFETY: management and Line exclusion prevent publication; Closing
        // rejects preparation. Every local teardown revoked its pointer after
        // its last complete invocation. take also rejects any remaining slot.
        let Some(owner) = (unsafe { line.nmi_action.take() }) else {
            return Ok(None);
        };
        entry.begin_teardown();
        mask(entry.desc);
        entry.finish_nmi_retirement();
        entry.remove_flags(IrqFlags::PER_CPU);
        Ok(Some(owner))
    });
    let was_removed = matches!(&result, Ok(Some(_)));
    if let Err(error) = &result
        && !crate::context::is_in_nmi()
    {
        warn!("NMI removal rejected: desc={desc:?}, error={error:?}");
    }
    // The original Line's management/admission guards have been released.
    drop(result);
    was_removed
}

/// No locks, owner clones, logging or destruction on the true NMI read path.
pub(super) fn dispatch_nmi_handler(source: IrqRef, scope: &krcu::ReadScope<'_>) {
    let Some(line) = super::index::borrowed(source, scope) else {
        return;
    };
    let Some(virq) = line.logical_irq() else {
        return;
    };
    line.nmi_action.with_current(|handler| {
        let Some(handler) = handler else { return };
        let _ = handler.handle(virq);
    });
}

unittest::path_mod_test!(tests, "../../ut-tests/nmi_management.rs");

#[cfg(unittest)]
pub use tests::NmiShutdownProbe;
