// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immediate routing for this singleton PLIC's ordinary external sources.
//!
//! Line exclusion precedes the PLIC lock. The transaction samples the source's
//! current context enable bit, suppresses the old context, moves the source's
//! recorded target context and establishes the requested delivery state on the
//! new context. It never changes source priority or KIRQ's logical disable
//! depth. Completion still belongs to the CPU that claimed the source.

use core::num::NonZeroU32;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use kirq::{IrqAffinityApplyError, IrqAffinityDelivery, IrqDesc};

use super::{PlicContexts, PlicRegisters, PlicSourceState, PlicState};

pub(super) static CHIP: kirq::IrqAffinityChip = kirq::IrqAffinityChip { apply };

trait Registers {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool;
    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool);
}

impl Registers for PlicRegisters {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        PlicRegisters::is_enabled(self, source, context)
    }

    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        PlicRegisters::set_enabled(self, source, context, enabled);
    }
}

fn apply(
    desc: IrqDesc,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    let controller = super::PLIC.get().ok_or(IrqAffinityApplyError::Rejected)?;
    if !matches!(
        super::select_plic_flow(desc, Some(controller.sources)),
        Ok(kirq::IrqFlow::Fasteoi(_))
    ) {
        return Err(IrqAffinityApplyError::Rejected);
    }
    let source = controller
        .sources
        .source(desc.hwirq)
        .ok_or(IrqAffinityApplyError::Rejected)?;

    let mut state = controller.state.lock();
    let PlicState {
        registers,
        sources,
        prepared,
    } = &mut *state;
    let source_state = sources
        .get_mut(desc.hwirq)
        .ok_or(IrqAffinityApplyError::Rejected)?;
    apply_locked(
        registers,
        source_state,
        source,
        candidates,
        delivery,
        |cpu| prepared_context(&controller.contexts, prepared, cpu),
    )
}

/// Production candidate filter: readiness alone is not enough; the CPU must
/// have completed local context preparation and have an authoritative mapping.
fn prepared_context(
    contexts: &PlicContexts,
    prepared: &KCpuMask,
    cpu: LogicalCpuId,
) -> Option<usize> {
    if !prepared.get_logical(cpu) {
        return None;
    }
    contexts.get(cpu).map(|context| context.index)
}

/// `source` is validated for this controller. Selection happens before any
/// register read or write; an unreachable/unprepared candidate set is rejected
/// with zero MMIO side effects.
fn apply_locked(
    registers: &mut impl Registers,
    state: &mut PlicSourceState,
    source: NonZeroU32,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
    mut resolve: impl FnMut(LogicalCpuId) -> Option<usize>,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    let (cpu, new_context) = candidates
        .iter_logical()
        .find_map(|cpu| resolve(cpu).map(|context| (cpu, context)))
        .ok_or(IrqAffinityApplyError::Rejected)?;

    let old_context = state.target_context;
    let was_enabled = old_context.is_some_and(|context| registers.is_enabled(source, context));

    if old_context == Some(new_context) {
        // Preserve an actually enabled route without a needless disable/enable
        // window. KeepMasked still has to establish suppression.
        if delivery == IrqAffinityDelivery::KeepMasked || !was_enabled {
            suppress(registers, source, new_context)?;
        }
        return Ok(KCpuMask::one_shot_logical(cpu));
    }

    if let Some(context) = old_context
        && was_enabled
    {
        suppress(registers, source, context)?;
    }

    // The old context is now suppressed (or was never enabled). Record the new
    // target before touching it so a later recovery disable closes the context
    // whose enable write may have failed. If old suppression failed above, the
    // recorded target remains the old context.
    state.target_context = Some(new_context);

    // A stale enable bit on the new context must not make Preserve claim an
    // enabled route when the incoming state was disabled, and KeepMasked must
    // verify suppression rather than assume preparation cleared it.
    suppress(registers, source, new_context)?;

    if delivery == IrqAffinityDelivery::Preserve && was_enabled {
        registers.set_enabled(source, new_context, true);
        if !registers.is_enabled(source, new_context) {
            // Request masking again without claiming the old route is restored
            // or that the source is quiescent.
            registers.set_enabled(source, new_context, false);
            return Err(IrqAffinityApplyError::HardwareFailure);
        }
    }

    Ok(KCpuMask::one_shot_logical(cpu))
}

/// Establishes suppression and verifies the enable bit without claiming that
/// an already-issued interrupt has drained.
fn suppress(
    registers: &mut impl Registers,
    source: NonZeroU32,
    context: usize,
) -> Result<(), IrqAffinityApplyError> {
    if registers.is_enabled(source, context) {
        registers.set_enabled(source, context, false);
        if registers.is_enabled(source, context) {
            registers.set_enabled(source, context, false);
            return Err(IrqAffinityApplyError::HardwareFailure);
        }
    }
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/plic_affinity.rs");
