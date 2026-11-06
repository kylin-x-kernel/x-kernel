// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Original-Line retention while a source owner coordinates local shutdown.

use alloc::sync::Arc;

use super::{ManagementMode, with_existing_line};
use crate::{IrqDesc, IrqDescError, IrqSpec, state::IrqLine};

/// Exclusive shutdown identity and actual local participants for one NMI action.
///
/// Wrap in an Arc when publishing CPU-local work. While any owner retains this
/// handle, ordinary unregister and replacement cannot remove its action. This
/// is not a CPU-online lease, producer stop or hardware drain. The source owner
/// must serialize producer restart and observe each local shutdown result.
///
/// Drop abandons only the coordinator claim, leaving the action in Closing for
/// retry; it does not re-enable or free hardware. Drop must not run in NMI.
/// Forgetting the handle deliberately prevents retirement rather than freeing
/// resources that outstanding work might still access.
pub struct NmiShutdown {
    line: Arc<IrqLine>,
    desc: IrqDesc,
    participants: [bool; kbuild_config::NR_CPUS],
}

impl NmiShutdown {
    /// Returns the retained descriptor, not a fresh numeric lookup.
    pub fn descriptor(&self) -> IrqDesc {
        self.desc
    }

    /// Whether this logical CPU had enabled or prepared local state at close.
    /// No new enable/preparation may extend this snapshot after admission closes.
    pub fn is_participant(&self, cpu: usize) -> bool {
        self.participants.get(cpu).copied().unwrap_or(false)
    }

    /// Retires the original action after completed source shutdown.
    ///
    /// Requires sleepable task context, but no action grace-period wait. Consume
    /// the unique handle only after remote callbacks and their captures finish;
    /// Arc owners must recover it with Arc::try_unwrap, not drop a clone and free
    /// by number. The source owner must also exclude stale hardware claims and
    /// producer restart. No remote work or source callbacks run inside KIRQ.
    ///
    /// # Errors
    /// Returns the unchanged handle on rejected context, incomplete local
    /// teardown, remaining local publication or stale identity. The action remains
    /// installed and admission stays closed, so the owner can retry.
    pub fn finish(self) -> Result<(), Self> {
        self.finish_with(|desc| crate::platform::set_platform_irq_enabled(desc, false))
    }

    pub(super) fn finish_with(self, mask: impl FnOnce(IrqDesc)) -> Result<(), Self> {
        if super::unregister_nmi_retained(self.desc, Some(&self.line), mask) {
            Ok(())
        } else {
            Err(self)
        }
    }
}

impl Drop for NmiShutdown {
    fn drop(&mut self) {
        self.line.lock().release_nmi_shutdown();
    }
}

/// Closes NMI enable/preparation and retains the original registration.
///
/// Task context only. No source is stopped and no IPI is sent. The returned
/// handle snapshots actual local obligations rather than the firmware CPU list.
/// An abandoned shutdown may be retried without reopening admission.
///
/// # Errors
/// Rejects invalid context, absent/non-NMI actions, unsupported non-PerCpu flows
/// and another outstanding shutdown handle before changing admission.
pub fn begin_nmi_shutdown(desc: IrqDesc) -> Result<NmiShutdown, IrqDescError> {
    let mut shutdown = None;
    with_existing_line(IrqSpec::Desc(desc), ManagementMode::Task, |virq, line| {
        let mut entry = line.lock();
        if !entry.has_nmi_action() {
            return Err(IrqDescError::NoIrqAction { virq });
        }
        if entry.flow != Some(crate::IrqFlow::PerCpu) {
            return Err(IrqDescError::UnsupportedFlow {
                hwirq: entry.desc.hwirq,
            });
        }
        if entry.has_nmi_shutdown() {
            return Err(IrqDescError::TeardownInProgress { virq });
        }
        shutdown = Some(NmiShutdown {
            line: line.clone(),
            desc: entry.desc,
            participants: entry.claim_nmi_shutdown(),
        });
        // This is not action removal: the helper must not finish teardown.
        Ok(None::<()>)
    })?;
    shutdown.ok_or(IrqDescError::UnknownIrq)
}
