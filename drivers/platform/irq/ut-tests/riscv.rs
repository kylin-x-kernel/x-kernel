// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use riscv::register::sie;
use unittest::{assert, assert_eq, def_test};

use super::{NonZeroU32, PlicCompletion, complete_plic_claim};

#[def_test]
fn plic_source_count_requires_one_supported_nonzero_cell() {
    use super::PlicSources;
    assert!(PlicSources::from_property(None).is_err());
    assert!(PlicSources::from_property(Some(&[])).is_err());
    assert!(PlicSources::from_property(Some(&[0, 0, 1])).is_err());
    assert!(PlicSources::from_property(Some(&[0; 8])).is_err());
    assert!(PlicSources::from_property(Some(&0u32.to_be_bytes())).is_err());
    assert!(PlicSources::from_property(Some(&1024u32.to_be_bytes())).is_err());
    assert!(PlicSources::from_property(Some(&u32::MAX.to_be_bytes())).is_err());
    assert_eq!(PlicSources::from_property(Some(&1u32.to_be_bytes())).unwrap().last, 1);
    assert_eq!(PlicSources::from_property(Some(&1023u32.to_be_bytes())).unwrap().last, 1023);
}

#[def_test]
fn plic_source_validation_rejects_zero_out_of_range_and_truncation() {
    let sources = super::PlicSources::from_property(Some(&53u32.to_be_bytes())).unwrap();
    assert!(sources.source(0).is_none());
    assert_eq!(sources.source(1).unwrap().get(), 1);
    assert_eq!(sources.source(53).unwrap().get(), 53);
    assert!(sources.source(54).is_none());
    assert!(sources.source(1023).is_none());
    assert!(sources.source(usize::MAX).is_none());
    assert!(sources.source((1usize << 32) + 1).is_none());
}

#[derive(Debug, PartialEq, Eq)]
enum CompletionAccess {
    Enable(bool),
    Complete,
}

#[def_test]
fn context_preparation_clears_supported_sources_without_completing_claims() {
    let mut model = CompletionModel {
        enabled: u32::MAX,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    super::disable_plic_sources(&mut model, 3, super::PlicSources { last: 7 });
    assert_eq!(model.enabled, !0xfeu32);
    assert_eq!(model.accepted, 0);
    assert!(model.is_claim_active);
    assert_eq!(model.accesses.len(), 7);
    assert!(model.accesses.iter().all(|access| *access == CompletionAccess::Enable(false)));
}

struct CompletionModel {
    enabled: u32,
    is_claim_active: bool,
    accepted: usize,
    accesses: alloc::vec::Vec<CompletionAccess>,
}

impl CompletionModel {
    fn claim_next(&mut self, source: NonZeroU32, context: usize) -> bool {
        if self.is_claim_active || !self.is_enabled(source, context) {
            return false;
        }
        self.is_claim_active = true;
        true
    }
}

impl PlicCompletion for CompletionModel {
    fn is_enabled(&self, source: NonZeroU32, context: usize) -> bool {
        core::assert_eq!(context, 3);
        self.enabled & (1 << source.get()) != 0
    }

    fn set_enabled(&mut self, source: NonZeroU32, context: usize, enabled: bool) {
        core::assert_eq!(context, 3);
        let bit = 1 << source.get();
        if enabled {
            self.enabled |= bit;
        } else {
            self.enabled &= !bit;
        }
        self.accesses.push(CompletionAccess::Enable(enabled));
    }

    fn complete(&mut self, context: usize, source: NonZeroU32) {
        // PLIC specification: a disabled target/source silently ignores completion.
        if self.is_enabled(source, context) {
            self.accepted += 1;
            self.is_claim_active = false;
        }
        self.accesses.push(CompletionAccess::Complete);
    }
}

#[def_test]
fn disabled_plic_completion_is_accepted_and_restores_enable_state() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: 1 << 7,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 1);
    assert!(!model.is_claim_active);
    assert_eq!(model.enabled, 1 << 7);
    assert_eq!(
        model.accesses.as_slice(),
        &[
            CompletionAccess::Enable(true),
            CompletionAccess::Complete,
            CompletionAccess::Enable(false),
        ]
    );
}

#[def_test]
fn enabled_plic_completion_does_not_toggle_enable_state() {
    let source = NonZeroU32::new(5).unwrap();
    let enabled = (1 << 5) | (1 << 7);
    let mut model = CompletionModel {
        enabled,
        is_claim_active: true,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 1);
    assert_eq!(model.enabled, enabled);
    assert_eq!(model.accesses.as_slice(), &[CompletionAccess::Complete]);
}

#[def_test]
fn plic_handler_disable_does_not_block_delivery_after_reenable() {
    let source = NonZeroU32::new(5).unwrap();
    let mut model = CompletionModel {
        enabled: (1 << 5) | (1 << 7),
        is_claim_active: false,
        accepted: 0,
        accesses: alloc::vec::Vec::new(),
    };
    assert!(model.claim_next(source, 3));
    model.set_enabled(source, 3, false);
    complete_plic_claim(&mut model, 3, source);
    assert!(!model.claim_next(source, 3));
    model.set_enabled(source, 3, true);
    assert!(model.claim_next(source, 3));
    complete_plic_claim(&mut model, 3, source);
    assert_eq!(model.accepted, 2);
    assert!(!model.is_claim_active);
    assert_eq!(model.enabled, (1 << 5) | (1 << 7));
}

#[def_test(serial)]
fn source_activation_preserves_global_irq_mask() {
    let _guard = kspin::NoPreemptIrqSave::new();
    let saved = sie::read();
    let mut masked = saved;
    masked.set_ssoft(false);
    masked.set_stimer(false);
    masked.set_sext(false);
    // SAFETY: Global IRQ delivery and migration are excluded; only this hart's
    // source enables change, and the saved state is restored before assertions.
    unsafe { sie::write(masked) };

    super::activate_local_interrupts();
    let activated = sie::read();
    let is_irq_enabled = karch::local_irq_enabled();

    // SAFETY: The guard still excludes delivery and migration on the same hart.
    unsafe { sie::write(saved) };

    let mut expected = saved;
    expected.set_ssoft(true);
    expected.set_stimer(true);
    expected.set_sext(true);
    assert_eq!(activated.bits(), expected.bits());
    assert!(!is_irq_enabled);
}
