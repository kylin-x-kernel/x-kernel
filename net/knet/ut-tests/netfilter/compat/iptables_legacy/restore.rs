// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;
use zerocopy::FromBytes;

use super::{
    super::{
        LegacyFilterTable,
        abi::IptEntry,
        decode::table_tests::{mixed_table, table_input},
    },
    *,
};
use crate::netfilter::filter::{
    config::FilterGeneration,
    counter::{ChainCounters, FilterCounter},
};

fn request(table: &LegacyFilterTable, num_counters: u32) -> LegacyFilterReplace<'_> {
    LegacyFilterReplace {
        table: table_input(table),
        num_counters,
    }
}

fn snapshot(config: &FilterConfig) -> (FilterGeneration, LegacyFilterTable) {
    config.with_state(|state| {
        (
            state.generation(),
            LegacyFilterTable::from_table(state.table()).unwrap(),
        )
    })
}

fn counted_table() -> FilterTable {
    let source = mixed_table();
    let updates: Vec<_> = source
        .chains()
        .iter()
        .enumerate()
        .map(|(index, chain)| ChainCounters {
            rules: chain
                .rules()
                .iter()
                .enumerate()
                .map(|(rule, _)| FilterCounter {
                    packets: (index * 10 + rule + 1) as u64,
                    bytes: (index * 100 + rule + 1) as u64,
                })
                .collect(),
            end: FilterCounter {
                packets: 100 + index as u64,
                bytes: 1000 + index as u64,
            },
        })
        .collect();
    source.add_counters(&updates).unwrap();
    source
}

#[def_test]
fn replacement_returns_final_legacy_counters_and_resets_new_counters() {
    let source = counted_table();
    let wire = LegacyFilterTable::from_table(&source).unwrap();
    let config = FilterConfig::new(source);
    let (generation, before) = snapshot(&config);
    let counters = request(&wire, wire.num_entries())
        .replace(&config, &MarkerCounters::default())
        .unwrap();
    assert_eq!(counters.len(), wire.num_entries() as usize);
    let mut offset = 0;
    for counter in counters {
        let entry = IptEntry::read_from_prefix(&wire.entries()[offset..])
            .unwrap()
            .0;
        assert_eq!(
            (counter.packets, counter.bytes),
            (entry.counters.packets, entry.counters.bytes)
        );
        offset += usize::from(entry.next_offset);
    }
    assert_eq!(offset, wire.entries().len());
    assert_ne!(snapshot(&config).0, generation);
    config.with_state(|state| {
        for counters in state.collect_counters() {
            assert_eq!(counters.end, FilterCounter::default());
            assert!(
                counters
                    .rules
                    .iter()
                    .all(|counter| *counter == FilterCounter::default())
            );
        }
    });
    assert_eq!(wire, before);
}

#[def_test]
fn failed_decode_and_old_count_conflicts_preserve_the_live_state() {
    let config = FilterConfig::new(counted_table());
    let before = snapshot(&config);
    let new = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    assert!(matches!(
        request(&new, 4).replace(&config, &MarkerCounters::default()),
        Err(RestoreError::CounterMismatch)
    ));
    assert_eq!(snapshot(&config), before);
    let input = LegacyFilterReplace {
        table: LegacyTableInput {
            entries: &new.entries()[..100],
            ..table_input(&new)
        },
        num_counters: before.1.num_entries(),
    };
    assert!(matches!(
        input.replace(&config, &MarkerCounters::default()),
        Err(RestoreError::Decode(_))
    ));
    assert_eq!(snapshot(&config), before);
    request(&new, before.1.num_entries())
        .replace(&config, &MarkerCounters::default())
        .unwrap();
    assert_eq!(snapshot(&config).1, new);
}

fn entry_counters(wire: &LegacyFilterTable) -> Vec<(u64, u64)> {
    let mut bytes = wire.entries();
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let entry = IptEntry::read_from_prefix(bytes).unwrap().0;
        result.push((entry.counters.packets, entry.counters.bytes));
        bytes = &bytes[usize::from(entry.next_offset)..];
    }
    result
}

#[def_test]
fn handle_projects_every_entry_and_returns_marker_counters_on_replace() {
    let config = FilterConfig::new(mixed_table());
    let markers = Mutex::new(MarkerCounters::default());
    let handle = LegacyTableHandle {
        config: &config,
        markers: &markers,
    };
    let info = handle.info().unwrap();
    let updates: Vec<_> = (0..info.num_entries)
        .map(|i| XtCounters {
            packets: u64::from(i) + 1,
            bytes: u64::from(i) + 100,
        })
        .collect();
    handle.add_counters(&updates).unwrap();
    handle.add_counters(&updates).unwrap();
    let exported = handle.export_entries(info.size).unwrap();
    let expected: Vec<_> = updates
        .iter()
        .map(|counter| (counter.packets * 2, counter.bytes * 2))
        .collect();
    assert_eq!(entry_counters(&exported), expected);
    let before = snapshot(&config);
    assert_eq!(
        handle.add_counters(&updates[..updates.len() - 1]),
        Err(KError::InvalidInput)
    );
    let new = LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap();
    assert_eq!(
        handle.replace(request(&new, 4)).err(),
        Some(LinuxError::EAGAIN.into())
    );
    let bad = LegacyFilterReplace {
        table: LegacyTableInput {
            entries: &new.entries()[..100],
            ..table_input(&new)
        },
        num_counters: info.num_entries,
    };
    assert_eq!(handle.replace(bad).err(), Some(KError::InvalidInput));
    assert_eq!(snapshot(&config), before);
    assert_eq!(
        entry_counters(&handle.export_entries(info.size).unwrap()),
        expected
    );
    let old = handle.replace(request(&new, info.num_entries)).unwrap();
    assert_eq!(
        old.iter().map(|v| (v.packets, v.bytes)).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        entry_counters(&handle.export_entries(632).unwrap()),
        alloc::vec![(0, 0); 4]
    );
    assert_eq!(entry_counters(&exported), expected);
}

#[def_test]
fn marker_totals_wrap_and_do_not_follow_an_unrelated_generation() {
    let config = FilterConfig::new(FilterTable::accept_all());
    let markers = Mutex::new(MarkerCounters::default());
    let handle = LegacyTableHandle {
        config: &config,
        markers: &markers,
    };
    handle
        .add_counters(
            &[XtCounters {
                packets: u64::MAX,
                bytes: u64::MAX,
            }; 4],
        )
        .unwrap();
    handle
        .add_counters(
            &[XtCounters {
                packets: 2,
                bytes: 3,
            }; 4],
        )
        .unwrap();
    assert_eq!(
        entry_counters(&handle.export_entries(632).unwrap()),
        alloc::vec![(1, 2); 4]
    );
    config
        .replace(FilterTable::accept_all(), |_, _| Ok(()))
        .unwrap();
    assert_eq!(
        entry_counters(&handle.export_entries(632).unwrap()),
        alloc::vec![(0, 0); 4]
    );
    handle
        .add_counters(
            &[XtCounters {
                packets: 5,
                bytes: 7,
            }; 4],
        )
        .unwrap();
    assert_eq!(
        entry_counters(&handle.export_entries(632).unwrap()),
        alloc::vec![(5, 7); 4]
    );
}
