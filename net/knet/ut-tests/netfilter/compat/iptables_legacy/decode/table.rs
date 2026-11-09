// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, vec};

use unittest::def_test;

use super::{
    super::{LegacyFilterTable, TableLayout},
    *,
};

pub(in super::super) fn table_input(table: &LegacyFilterTable) -> LegacyTableInput<'_> {
    LegacyTableInput {
        valid_hooks: table.valid_hooks(),
        hook_entry: *table.hook_entry(),
        underflow: *table.underflow(),
        num_entries: table.num_entries(),
        entries: table.entries(),
    }
}

fn rule(target: Target) -> Rule {
    Rule::new(BaseMatch::default(), vec![], target).unwrap()
}

pub(in super::super) fn mixed_table() -> FilterTable {
    FilterTable::new(vec![
        Chain::user_defined("B".to_string(), vec![rule(Target::Return)]).unwrap(),
        Chain::built_in(
            FilterHook::LocalOut,
            Verdict::Drop,
            vec![
                Rule::new(
                    BaseMatch {
                        protocol: Some(Negated::new(6, false)),
                        ..Default::default()
                    },
                    vec![ExtensionMatch::Tcp(TcpMatch::new())],
                    Target::Jump(ChainId::from_index(2)),
                )
                .unwrap(),
            ],
        ),
        Chain::user_defined(
            "A".to_string(),
            vec![
                rule(Target::Jump(ChainId::from_index(0))),
                rule(Target::Jump(ChainId::from_index(4))),
                rule(Target::Continue),
            ],
        )
        .unwrap(),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, vec![]),
        Chain::user_defined("EMPTY".to_string(), vec![]).unwrap(),
        Chain::built_in(
            FilterHook::LocalIn,
            Verdict::Accept,
            vec![
                rule(Target::Jump(ChainId::from_index(2))),
                rule(Target::Continue),
            ],
        ),
    ])
    .unwrap()
}

fn set_target(bytes: &mut [u8], offset: usize, verdict: i32) {
    let header = IptEntry::read_from_prefix(&bytes[offset..]).unwrap().0;
    let offset = offset + usize::from(header.target_offset) + 32;
    bytes[offset..offset + 4].copy_from_slice(&verdict.to_ne_bytes());
}

#[def_test]
fn native_default_and_reordered_tables_round_trip() {
    for source in [FilterTable::accept_all(), mixed_table()] {
        let wire = LegacyFilterTable::from_table(&source).unwrap();
        let decoded = FilterTable::try_from(&table_input(&wire)).unwrap();
        assert_eq!(LegacyFilterTable::from_table(&decoded).unwrap(), wire);
        assert_eq!(decoded.chains().len(), source.chains().len());
        for (actual, expected) in decoded.chains().iter().zip(source.chains()) {
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.rules().len(), expected.rules().len());
            for (actual, expected) in actual.rules().iter().zip(expected.rules()) {
                assert_eq!(actual.target(), expected.target());
            }
        }
    }
}

#[def_test]
fn hook_offsets_must_describe_distinct_complete_builtin_chains() {
    let wire = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    for (hook, underflow) in [(8, 0), (152, 0), (0, 152), (456, 0)] {
        let mut input = table_input(&wire);
        input.hook_entry[1] = hook;
        input.underflow[1] = underflow;
        assert!(matches!(
            FilterTable::try_from(&input),
            Err(DecodeError::InvalidLayout)
        ));
    }
    let mut input = table_input(&wire);
    input.valid_hooks = 0x0f;
    assert!(matches!(
        FilterTable::try_from(&input),
        Err(DecodeError::InvalidLayout)
    ));
    let mut input = table_input(&wire);
    input.hook_entry[0] = u32::MAX;
    input.underflow[4] = u32::MAX;
    assert!(FilterTable::try_from(&input).is_ok());
}

#[def_test]
fn targets_must_be_next_entries_or_user_chain_entries() {
    let source = mixed_table();
    let wire = LegacyFilterTable::from_table(&source).unwrap();
    let layout = TableLayout::new(&source).unwrap();
    for target in [
        layout.chains[0].head_offset,
        layout.chains[1].rules_offset,
        layout.chains[1].rules_offset + STANDARD_ENTRY_SIZE,
        layout.chains[2].rules_offset + STANDARD_ENTRY_SIZE,
        layout.chains[2].rules_offset + 8,
        wire.entries().len() - super::super::abi::ERROR_ENTRY_SIZE,
        wire.entries().len(),
    ] {
        let mut bytes = wire.entries().to_vec();
        set_target(&mut bytes, layout.chains[5].rules_offset, target as i32);
        let input = LegacyTableInput {
            entries: &bytes,
            ..table_input(&wire)
        };
        assert!(matches!(
            FilterTable::try_from(&input),
            Err(DecodeError::InvalidLayout)
        ));
    }
}

#[def_test]
fn semantic_validation_rejects_cycles_duplicate_names_and_protocol_conflicts() {
    let source = mixed_table();
    let wire = LegacyFilterTable::from_table(&source).unwrap();
    let layout = TableLayout::new(&source).unwrap();
    let mut cycle = wire.entries().to_vec();
    set_target(
        &mut cycle,
        layout.chains[0].rules_offset,
        layout.chains[2].rules_offset as i32,
    );
    let mut duplicate = wire.entries().to_vec();
    duplicate[layout.chains[0].head_offset + 144] = b'A';
    let mut protocol = wire.entries().to_vec();
    let offset = layout.chains[1].rules_offset + 80;
    protocol[offset..offset + 2].copy_from_slice(&17u16.to_ne_bytes());
    for bytes in [cycle, duplicate, protocol] {
        let input = LegacyTableInput {
            entries: &bytes,
            ..table_input(&wire)
        };
        assert!(matches!(
            FilterTable::try_from(&input),
            Err(DecodeError::InvalidTable)
        ));
    }
}

#[def_test]
fn tails_and_terminal_markers_are_required_and_unconditional() {
    let source = mixed_table();
    let wire = LegacyFilterTable::from_table(&source).unwrap();
    let layout = TableLayout::new(&source).unwrap();
    let mut conditional = wire.entries().to_vec();
    conditional[layout.chains[1].tail_offset + 80] = 6;
    let mut wrong_policy = wire.entries().to_vec();
    set_target(&mut wrong_policy, layout.chains[1].tail_offset, -5);
    let mut missing_return = wire.entries().to_vec();
    set_target(&mut missing_return, layout.chains[4].tail_offset, -2);
    let mut terminal = wire.entries().to_vec();
    let last_name = terminal.len() - super::super::abi::ERROR_ENTRY_SIZE + 144;
    terminal[last_name] = b'X';
    for bytes in [conditional, wrong_policy, missing_return, terminal] {
        let input = LegacyTableInput {
            entries: &bytes,
            ..table_input(&wire)
        };
        assert!(matches!(
            FilterTable::try_from(&input),
            Err(DecodeError::InvalidLayout)
        ));
    }
}

#[def_test]
fn entry_counts_and_resource_limits_are_checked_before_publication() {
    let wire = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    for count in [0, 3, 5, u32::MAX] {
        let input = LegacyTableInput {
            num_entries: count,
            ..table_input(&wire)
        };
        assert!(FilterTable::try_from(&input).is_err());
    }
    let bytes = vec![0; MAX_BYTES + 1];
    let input = LegacyTableInput {
        entries: &bytes,
        ..table_input(&wire)
    };
    assert!(matches!(
        FilterTable::try_from(&input),
        Err(DecodeError::TooLarge)
    ));
    let input = LegacyTableInput {
        entries: &wire.entries()[..wire.entries().len() - 1],
        ..table_input(&wire)
    };
    assert!(matches!(
        FilterTable::try_from(&input),
        Err(DecodeError::InvalidLayout)
    ));
}
