// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::string::ToString;

use unittest::def_test;
use zerocopy::FromBytes;

use super::*;
use crate::netfilter::filter::{
    chain::{Chain, ChainId},
    counter::FilterCounter,
    matcher::{ExtensionMatch, Negated, TcpMatch},
    rule::{Rule, Verdict},
};

fn rule(target: Target) -> Rule {
    Rule::new(BaseMatch::default(), Vec::new(), target).unwrap()
}

fn mixed_table() -> FilterTable {
    let tcp = Rule::new(
        BaseMatch {
            protocol: Some(Negated::new(6, false)),
            ..Default::default()
        },
        vec![ExtensionMatch::Tcp(TcpMatch::new())],
        Target::Jump(ChainId::from_index(3)),
    )
    .unwrap();
    FilterTable::new(vec![
        Chain::user_defined("B".to_string(), vec![rule(Target::Return)]).unwrap(),
        Chain::built_in(
            FilterHook::LocalOut,
            Verdict::Drop,
            vec![tcp, rule(Target::Continue)],
        ),
        Chain::user_defined("EMPTY".to_string(), Vec::new()).unwrap(),
        Chain::user_defined(
            "A".to_string(),
            vec![
                rule(Target::Jump(ChainId::from_index(0))),
                rule(Target::Jump(ChainId::from_index(2))),
                rule(Target::Continue),
            ],
        )
        .unwrap(),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
        Chain::built_in(
            FilterHook::LocalIn,
            Verdict::Accept,
            vec![
                rule(Target::Jump(ChainId::from_index(3))),
                rule(Target::Return),
            ],
        ),
    ])
    .unwrap()
}

fn entry_at(table: &LegacyFilterTable, offset: usize) -> abi::IptEntry {
    abi::IptEntry::read_from_prefix(&table.entries()[offset..])
        .unwrap()
        .0
}

fn verdict_at(table: &LegacyFilterTable, offset: usize) -> i32 {
    let entry = entry_at(table, offset);
    abi::XtStandardTarget::read_from_prefix(
        &table.entries()[offset + usize::from(entry.target_offset)..],
    )
    .unwrap()
    .0
    .verdict
}

fn entry_offsets(table: &LegacyFilterTable) -> Vec<usize> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < table.entries().len() {
        result.push(offset);
        let entry = entry_at(table, offset);
        assert!(entry.next_offset >= 152);
        assert_eq!(entry.next_offset % 8, 0);
        offset += usize::from(entry.next_offset);
    }
    assert_eq!(offset, table.entries().len());
    assert_eq!(result.len(), table.num_entries() as usize);
    result
}

#[def_test]
fn default_table_matches_native_initial_layout() {
    let table = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    assert_eq!(table.valid_hooks(), 0x0e);
    assert_eq!(table.hook_entry(), &[0, 0, 152, 304, 0]);
    assert_eq!(table.underflow(), table.hook_entry());
    assert_eq!(table.num_entries(), 4);
    assert_eq!(table.entries().len(), 632);
    assert_eq!(entry_offsets(&table), vec![0, 152, 304, 456]);
    for offset in [0, 152, 304] {
        assert_eq!(entry_at(&table, offset).target_offset, 112);
        assert_eq!(verdict_at(&table, offset), -2);
    }
    assert_eq!(&table.entries()[570..575], b"ERROR");
    assert_eq!(&table.entries()[600..605], b"ERROR");
}

#[def_test]
fn reordered_chains_resolve_jumps_and_empty_chain_tails() {
    let table = LegacyFilterTable::from_table(&mixed_table()).unwrap();
    assert_eq!(table.hook_entry(), &[0, 2248, 2096, 480, 0]);
    assert_eq!(table.underflow(), &[0, 2552, 2096, 832, 0]);
    assert_eq!(table.entries().len(), 2880);
    assert_eq!(
        entry_offsets(&table),
        vec![
            0, 176, 328, 480, 680, 832, 984, 1160, 1312, 1488, 1640, 1792, 1944, 2096, 2248, 2400,
            2552, 2704,
        ]
    );
    for (offset, verdict) in [
        (176, -5),
        (328, -5),
        (480, 1488),
        (680, 832),
        (832, -1),
        (1160, -5),
        (1488, 176),
        (1640, 1160),
        (1792, 1944),
        (1944, -5),
        (2096, -2),
        (2248, 1488),
        (2400, -5),
        (2552, -2),
    ] {
        assert_eq!(verdict_at(&table, offset), verdict);
    }
    assert_eq!(entry_at(&table, 480).target_offset, 160);
    for (offset, name) in [(0, "B"), (984, "EMPTY"), (1312, "A"), (2704, "ERROR")] {
        let marker = abi::XtErrorTarget::read_from_prefix(&table.entries()[offset + 112..])
            .unwrap()
            .0;
        assert_eq!(&marker.header.name[..5], b"ERROR");
        assert_eq!(&marker.name[..name.len()], name.as_bytes());
    }
}

#[def_test]
fn table_offsets_use_the_full_native_jump_width() {
    let mut rules: Vec<_> = (0..450).map(|_| rule(Target::Continue)).collect();
    rules.push(rule(Target::Jump(ChainId::from_index(3))));
    let table = FilterTable::new(vec![
        Chain::built_in(FilterHook::LocalIn, Verdict::Accept, rules),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
        Chain::user_defined("EMPTY".to_string(), Vec::new()).unwrap(),
    ])
    .unwrap();
    let table = LegacyFilterTable::from_table(&table).unwrap();
    assert_eq!(table.underflow()[1], 68552);
    assert_eq!(table.hook_entry()[3], 68856);
    assert_eq!(verdict_at(&table, 450 * 152), 69184);
    assert_eq!(verdict_at(&table, 69184), -5);
    assert_eq!(entry_at(&table, 450 * 152).next_offset, 152);
}

#[def_test]
fn counters_follow_rules_and_tails_and_exports_own_their_bytes() {
    let table = mixed_table();
    let mut counters = table.collect_counters();
    let mut packets = 1;
    for chain in &mut counters {
        for counter in chain
            .rules
            .iter_mut()
            .chain(core::iter::once(&mut chain.end))
        {
            *counter = FilterCounter {
                packets,
                bytes: packets * 123,
            };
            packets += 1;
        }
    }
    table.add_counters(&counters).unwrap();
    let exported = LegacyFilterTable::from_table(&table).unwrap();
    let before = exported.entries().to_vec();
    let expected = [0, 1, 2, 3, 4, 5, 0, 6, 0, 7, 8, 9, 10, 11, 12, 13, 14, 0];
    for (offset, packets) in entry_offsets(&exported).into_iter().zip(expected) {
        let counter = entry_at(&exported, offset).counters;
        assert_eq!(counter.packets, packets);
        assert_eq!(counter.bytes, packets * 123);
    }
    let replacement = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    assert_eq!(replacement.num_entries(), 4);
    for offset in entry_offsets(&replacement) {
        let counter = entry_at(&replacement, offset).counters;
        assert_eq!((counter.packets, counter.bytes), (0, 0));
    }
    assert_eq!(exported.entries(), before);
    assert_eq!(LegacyFilterTable::from_table(&table).unwrap(), exported);
}

#[def_test]
fn encoding_errors_preserve_table_counters() {
    let invalid = Rule::new(
        BaseMatch {
            protocol: Some(Negated::new(0, false)),
            ..Default::default()
        },
        Vec::new(),
        Target::Continue,
    )
    .unwrap();
    let table = FilterTable::new(vec![
        Chain::built_in(FilterHook::Forward, Verdict::Drop, Vec::new()),
        Chain::built_in(FilterHook::LocalIn, Verdict::Accept, vec![invalid]),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
    ])
    .unwrap();
    let mut counters = table.collect_counters();
    counters[0].end = FilterCounter {
        packets: 7,
        bytes: 28,
    };
    table.add_counters(&counters).unwrap();
    assert_eq!(
        LegacyFilterTable::from_table(&table).err(),
        Some(EncodeError::UnsupportedMatch)
    );
    assert_eq!(table.collect_counters(), counters);
    assert_eq!(
        table.chains()[1].rules()[0].base().protocol,
        Some(Negated::new(0, false))
    );
}
