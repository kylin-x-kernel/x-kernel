// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::vec::Vec;
use core::num::NonZeroU32;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use kirq::{IrqAffinityApplyError, IrqAffinityDelivery};
use unittest::{assert, assert_eq, def_test};

use super::super::PlicSourceState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Enable { context: usize, enabled: bool },
}

struct Model {
    contexts: Vec<bool>,
    priorities: Vec<u32>,
    completed: Vec<usize>,
    accesses: Vec<Access>,
    disable_failures: Vec<u8>,
    enable_failures: Vec<bool>,
}

impl Model {
    fn new(contexts: usize) -> Self {
        Self {
            contexts: alloc::vec![false; contexts],
            priorities: alloc::vec![0; 8],
            completed: Vec::new(),
            accesses: Vec::new(),
            disable_failures: alloc::vec![0; contexts],
            enable_failures: alloc::vec![false; contexts],
        }
    }

    fn set_context(&mut self, context: usize, enabled: bool) {
        self.contexts[context] = enabled;
    }

    fn context(&self, context: usize) -> bool {
        self.contexts[context]
    }

    fn priority(&self, source: usize) -> u32 {
        self.priorities[source]
    }
}

impl super::Registers for Model {
    fn is_enabled(&self, _: NonZeroU32, context: usize) -> bool {
        self.contexts[context]
    }

    fn set_enabled(&mut self, _: NonZeroU32, context: usize, enabled: bool) {
        self.accesses.push(Access::Enable { context, enabled });
        if enabled {
            if !self.enable_failures[context] {
                self.contexts[context] = true;
            }
        } else if self.disable_failures[context] > 0 {
            self.disable_failures[context] -= 1;
        } else {
            self.contexts[context] = false;
        }
    }
}

impl super::super::PlicPriorityRegisters for Model {
    fn read_priority(&self, source: NonZeroU32) -> u32 {
        self.priorities[source.get() as usize]
    }

    fn write_priority(&mut self, source: NonZeroU32, priority: u32) {
        self.priorities[source.get() as usize] = priority;
    }
}

impl super::super::PlicCompletion for Model {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        <Self as super::Registers>::is_enabled(self, source, context)
    }

    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        <Self as super::Registers>::set_enabled(self, source, context, enabled);
    }

    fn complete(&mut self, context: usize, _: NonZeroU32) {
        self.completed.push(context);
    }
}

fn source() -> NonZeroU32 {
    NonZeroU32::new(5).unwrap()
}

fn state(target_context: Option<usize>) -> PlicSourceState {
    PlicSourceState {
        target_context,
        configured_priority: Some(6),
        is_masked: false,
    }
}

fn candidates(cpus: &[usize]) -> KCpuMask {
    let mut mask = KCpuMask::new();
    for &cpu in cpus {
        mask.set_logical(LogicalCpuId::new(cpu), true);
    }
    mask
}

fn apply(
    model: &mut Model,
    state: &mut PlicSourceState,
    cpus: &[usize],
    delivery: IrqAffinityDelivery,
    resolve: impl FnMut(LogicalCpuId) -> Option<usize>,
) -> Result<KCpuMask, IrqAffinityApplyError> {
    super::apply_locked(model, state, source(), &candidates(cpus), delivery, resolve)
}

fn recover_after_failure(model: &mut Model, state: &mut PlicSourceState) -> unittest::TestResult {
    state.set_enabled(model, 0, source(), false);
    assert!(!model.context(2));
    assert!(!model.context(3));

    let _ = apply(
        model,
        state,
        &[0],
        IrqAffinityDelivery::KeepMasked,
        |_| Some(3),
    )
    .unwrap();
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(!model.context(3));

    model.enable_failures[3] = false;
    state.set_enabled(model, 0, source(), true);
    assert!(!model.context(2));
    assert!(model.context(3));
    unittest::TestResult::Ok
}

#[def_test]
fn rejected_candidate_set_has_zero_register_writes() {
    let mut model = Model::new(4);
    let mut state = state(None);
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| None,
    );
    assert_eq!(result, Err(IrqAffinityApplyError::Rejected));
    assert!(model.accesses.is_empty());
    assert_eq!(state.target_context, None);
}

#[def_test]
fn selection_skips_unresolvable_candidates_before_any_write() {
    if kbuild_config::NR_CPUS < 2 {
        return unittest::TestResult::Ok;
    }
    let mut model = Model::new(4);
    let mut state = state(None);
    let result = apply(
        &mut model,
        &mut state,
        &[0, 1],
        IrqAffinityDelivery::Preserve,
        |cpu| (cpu.as_usize() == 1).then_some(3),
    )
    .unwrap();
    assert!(result.get_logical(LogicalCpuId::new(1)));
    assert_eq!(result.iter_logical().count(), 1);
    assert_eq!(state.target_context, Some(3));
    assert!(model.accesses.is_empty());
}

#[def_test]
fn prepared_context_filter_requires_mask_and_authoritative_mapping() {
    let declarations = [super::super::context::PlicContextDecl {
        index: 3,
        hardware_cpu_id: 0x100,
    }];
    let contexts = super::super::PlicContexts::build(&declarations, 0x0400_0000, |_| {
        Some(LogicalCpuId::new(0))
    })
    .unwrap();
    let mut prepared = KCpuMask::new();
    assert_eq!(
        super::prepared_context(&contexts, &prepared, LogicalCpuId::new(0)),
        None
    );

    prepared.set_logical(LogicalCpuId::new(0), true);
    assert_eq!(
        super::prepared_context(&contexts, &prepared, LogicalCpuId::new(0)),
        Some(3)
    );

    let empty = super::super::PlicContexts::new();
    assert_eq!(
        super::prepared_context(&empty, &prepared, LogicalCpuId::new(0)),
        None
    );
}

#[def_test]
fn preserve_moves_an_enabled_source_to_the_new_context() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    )
    .unwrap();
    assert!(result.get_logical(LogicalCpuId::new(0)));
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(model.context(3));
    assert_eq!(
        model.accesses,
        [
            Access::Enable {
                context: 2,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: true,
            },
        ]
    );
    assert_eq!(state.configured_priority, Some(6));
    assert!(!state.is_masked);
}

#[def_test]
fn keep_masked_leaves_the_new_context_disabled() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::KeepMasked,
        |_| Some(3),
    )
    .unwrap();
    assert!(result.get_logical(LogicalCpuId::new(0)));
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(!model.context(3));
    assert_eq!(
        model.accesses,
        [Access::Enable {
            context: 2,
            enabled: false,
        }]
    );
    assert_eq!(state.configured_priority, Some(6));
    assert!(!state.is_masked);
}

#[def_test]
fn preserve_does_not_enable_a_previously_disabled_source() {
    let mut model = Model::new(4);
    let mut state = state(Some(2));
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    )
    .unwrap();
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(!model.context(3));
    assert!(model.accesses.is_empty());
}

#[def_test]
fn preserve_same_enabled_target_does_not_disable_delivery() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = state(Some(2));
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(2),
    )
    .unwrap();
    assert_eq!(state.target_context, Some(2));
    assert!(model.context(2));
    assert!(model.accesses.is_empty());
}

#[def_test]
fn keep_masked_same_target_still_suppresses_and_preserves_priority_policy() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::KeepMasked,
        |_| Some(2),
    )
    .unwrap();
    assert!(result.get_logical(LogicalCpuId::new(0)));
    assert!(!model.context(2));
    assert_eq!(
        model.accesses,
        [Access::Enable {
            context: 2,
            enabled: false,
        }]
    );
    assert_eq!(state.configured_priority, Some(6));
    assert!(!state.is_masked);
}

#[def_test]
fn old_disable_failure_keeps_the_old_target_and_does_not_touch_new_context() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    model.disable_failures[2] = 2;
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    );
    assert_eq!(result, Err(IrqAffinityApplyError::HardwareFailure));
    assert_eq!(state.target_context, Some(2));
    assert!(model.context(2));
    assert!(!model.context(3));
    assert_eq!(
        model.accesses,
        [
            Access::Enable {
                context: 2,
                enabled: false,
            },
            Access::Enable {
                context: 2,
                enabled: false,
            },
        ]
    );
    assert!(
        recover_after_failure(&mut model, &mut state) == unittest::TestResult::Ok
    );
}

#[def_test]
fn new_enable_failure_requests_new_context_masking_and_records_new_target() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    model.enable_failures[3] = true;
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    );
    assert_eq!(result, Err(IrqAffinityApplyError::HardwareFailure));
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(!model.context(3));
    assert_eq!(
        model.accesses,
        [
            Access::Enable {
                context: 2,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: true,
            },
            Access::Enable {
                context: 3,
                enabled: false,
            },
        ]
    );
    assert!(
        recover_after_failure(&mut model, &mut state) == unittest::TestResult::Ok
    );
}

#[def_test]
fn new_context_suppression_failure_records_new_target_and_requests_masking() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    model.set_context(3, true);
    model.disable_failures[3] = 2;
    let mut state = state(Some(2));
    let result = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::KeepMasked,
        |_| Some(3),
    );
    assert_eq!(result, Err(IrqAffinityApplyError::HardwareFailure));
    assert_eq!(state.target_context, Some(3));
    assert!(!model.context(2));
    assert!(model.context(3));
    assert_eq!(
        model.accesses,
        [
            Access::Enable {
                context: 2,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: false,
            },
        ]
    );
    assert!(
        recover_after_failure(&mut model, &mut state) == unittest::TestResult::Ok
    );
}

#[def_test]
fn preserve_suppresses_stale_new_context_before_enabling() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    model.set_context(3, true);
    let mut state = state(Some(2));
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    )
    .unwrap();
    assert!(!model.context(2));
    assert!(model.context(3));
    assert_eq!(
        model.accesses,
        [
            Access::Enable {
                context: 2,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: false,
            },
            Access::Enable {
                context: 3,
                enabled: true,
            },
        ]
    );
}

#[def_test]
fn preserve_migration_keeps_masked_nondefault_priority_policy() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = PlicSourceState {
        target_context: Some(2),
        configured_priority: Some(17),
        is_masked: true,
    };
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    )
    .unwrap();
    assert_eq!(state.target_context, Some(3));
    assert_eq!(state.configured_priority, Some(17));
    assert!(state.is_masked);
    assert_eq!(model.priority(5), 0);
    assert!(!model.context(2));
    assert!(model.context(3));
}

#[def_test]
fn old_claim_completes_on_old_context_after_migration() {
    let mut model = Model::new(4);
    model.set_context(2, true);
    let mut state = state(Some(2));
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::Preserve,
        |_| Some(3),
    )
    .unwrap();
    assert!(model.context(3));

    state.set_enabled(&mut model, 0, source(), false);
    assert!(!model.context(2));
    assert!(!model.context(3));

    super::super::complete_plic_claim(&mut model, 2, source());
    assert_eq!(model.completed, [2]);
    assert!(!model.context(2));
    assert!(!model.context(3));
    assert_eq!(state.target_context, Some(3));
}

#[def_test]
fn later_enable_uses_the_affinity_target_not_the_management_cpu() {
    let mut model = Model::new(4);
    let mut state = state(None);
    let _ = apply(
        &mut model,
        &mut state,
        &[0],
        IrqAffinityDelivery::KeepMasked,
        |_| Some(3),
    )
    .unwrap();
    assert_eq!(state.target_context, Some(3));

    state.set_enabled(&mut model, 1, source(), true);
    assert_eq!(state.target_context, Some(3));
    assert!(model.context(3));
    assert!(!model.context(1));
    assert!(!model.context(2));
    assert_eq!(model.priority(5), 6);
}
