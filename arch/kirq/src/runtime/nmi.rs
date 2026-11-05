// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! NMI registration and dispatch state.

use kalloc_types::KBox;
use kspin::NoPreemptIrqSave;

mod shutdown;
pub use shutdown::{NmiShutdown, begin_nmi_shutdown};

use super::manager::{
    ManagementMode, validate_management, with_existing_line, with_registration_line_checked,
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
    with_local_nmi(spec.into(), |virq, entry| {
        if entry.is_nmi_closing() || entry.is_teardown_in_progress() {
            return Err(IrqDescError::TeardownInProgress { virq });
        }
        let chip = IntrManagerIf::percpu_nmi(entry.desc).ok_or(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        })?;
        entry.prepare_local_nmi(chip)
    })
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
    with_local_nmi(spec.into(), |_, entry| entry.teardown_local_nmi())
}

fn with_local_nmi(
    spec: IrqSpec,
    operation: impl FnOnce(usize, &mut crate::state::IrqStateDesc) -> Result<(), IrqDescError>,
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
    if entry.flow != crate::IrqFlow::PerCpu {
        return Err(IrqDescError::UnsupportedFlow {
            hwirq: entry.desc.hwirq,
        });
    }
    operation(virq, &mut entry)
}

/// Registers the sole Line-owned handler for true NMI and ordinary IRQ fallback.
///
/// Requires a previously mapped descriptor and sleepable task context. The
/// source owner must keep delivery stopped until registration and CPU-local
/// preparation are complete. This tags the Line PER_CPU, but neither starts the
/// producer nor enables a CPU's source. Duplicate/regular/shared ownership is
/// rejected. Only PerCpu and Legacy fallback flows are supported. Hardware
/// configuration failure publishes no handler; partially
/// written hardware remains the source owner's cleanup responsibility.
///
/// Dispatch uses a typed Domain lookup and an RCU borrow. No exclusion of
/// unrelated NMI sources is required. Handler destruction never runs in NMI.
pub fn register_nmi(desc: IrqDesc, handler: Handler) -> bool {
    register_nmi_with_mode(desc, handler, ManagementMode::Task, configure_platform_irq)
}

/// Registers an NMI handler during allocator-ready startup without waiting.
///
/// Requires an installed task outside IRQ/BH/NMI context. IRQs may be masked;
/// management contention fails instead of sleeping. Mapping and source shutdown
/// prerequisites are the same as for [`register_nmi`]. Publication needs no
/// running RCU service; removal does require that service.
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
    let result = validate_management(mode).and_then(|()| {
        // Keep the offered allocation outside the helper so rejection cannot
        // finally destroy a client's handler under management or short locks.
        let mut prepared = Some(KBox::new_nowait(handler).map_err(|_| IrqDescError::NoMemory)?);
        with_registration_line_checked(
            IrqSpec::Desc(desc.with_flags(IrqFlags::PER_CPU)),
            mode,
            |entry| Ok(entry.can_install_regular_action()),
            |_, line, entry| {
                if !matches!(entry.flow, crate::IrqFlow::PerCpu | crate::IrqFlow::Legacy) {
                    return Err(IrqDescError::UnsupportedFlow {
                        hwirq: entry.desc.hwirq,
                    });
                }
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

/// Removes a Line's NMI action after CPU-local shutdown and an RCU grace period.
///
/// Requires sleepable task context and an operational RCU service. The source
/// owner must stop event generation, serialize against restart, and complete
/// disable plus [`teardown_percpu_nmi`] on every prepared CPU. Enqueuing remote
/// work is not completion. Pending/active hardware claims must not cross reuse.
///
/// Removal closes enable/preparation admission. An enabled or prepared slot
/// returns false, retaining ownership and Closing so local cleanup can finish.
/// Once all slots are clear, withdrawal prevents new borrows and waits for old
/// true-NMI/fallback scopes, including original claim completion. Only then may
/// another registration use the Line. Destruction runs after management unlock.
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
    let result = with_existing_line(IrqSpec::Desc(desc), ManagementMode::Task, |_, line| {
        if retained.is_some_and(|original| !alloc::sync::Arc::ptr_eq(original, line)) {
            return Ok(None);
        }
        let prepared = line.nmi_action.with_prepared_take(|take| {
            let retired = {
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
                // Keep the action marker and Closing through GP completion.
                // No new owner or local preparation can overtake retirement.
                entry.begin_teardown();
                mask(entry.desc);
                take.withdraw()
            };
            // SAFETY: the preparing task has restored watched sleepable context.
            // Only management exclusion remains, which neither RCU progress nor
            // IRQ/NMI readers acquire. The original Line remains retained.
            let owner = unsafe { retired.reclaim() };
            {
                let mut entry = line.lock();
                entry.finish_nmi_retirement();
                entry.remove_flags(IrqFlags::PER_CPU);
            }
            Ok(Some(owner))
        });
        match prepared {
            Ok(result) => result,
            Err(error) => {
                warn!("NMI retirement preparation failed: desc={desc:?}, error={error:?}");
                Ok(None)
            }
        }
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
    if let Some(handler) = line.nmi_action.read(scope) {
        let hwirq = match source {
            IrqRef::Domain(_, hwirq) => hwirq,
            IrqRef::Virq(virq) => virq,
        };
        let _ = handler.handle(hwirq);
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/nmi_management.rs");

#[cfg(unittest)]
pub use tests::NmiShutdownProbe;
