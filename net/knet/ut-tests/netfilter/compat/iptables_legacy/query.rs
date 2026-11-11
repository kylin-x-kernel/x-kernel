// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use ksync::Mutex;
use unittest::def_test;
use zerocopy::FromBytes;

use super::*;
use crate::netfilter::{
    compat::iptables_legacy::{
        EXTENSION_NAME_LEN, ExtensionKind, TABLE_NAME_LEN, abi::IptEntry, restore::MarkerCounters,
    },
    filter::{
        chain::{Chain, FilterHook},
        config::FilterConfig,
        counter::FilterCounter,
        matcher::{BaseMatch, Negated},
        rule::{Rule, Target, Verdict},
        table::FilterTable,
    },
};

fn table(policy: Verdict, rules: Vec<Rule>) -> FilterTable {
    FilterTable::new(vec![
        Chain::built_in(FilterHook::LocalIn, policy, rules),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
    ])
    .unwrap()
}

#[def_test]
fn queries_follow_current_generation_and_exports_remain_owned() {
    let config = FilterConfig::new(FilterTable::accept_all());
    let markers = Mutex::new(MarkerCounters::default());
    let reader = LegacyTableHandle {
        config: &config,
        markers: &markers,
    };
    let info = reader.info().unwrap();
    assert_eq!(&info.name[..7], b"filter\0");
    assert_eq!(info.valid_hooks, 0x0e);
    assert_eq!(info.hook_entry, [0, 0, 152, 304, 0]);
    assert_eq!(info.underflow, info.hook_entry);
    assert_eq!((info.num_entries, info.size), (4, 632));
    let exported = reader.export_entries(info.size).unwrap();
    assert_eq!(
        exported,
        LegacyFilterTable::from_table(&FilterTable::accept_all()).unwrap()
    );

    config
        .replace(table(Verdict::Drop, Vec::new()), |_, _| Ok(()))
        .unwrap();
    assert_eq!(reader.info().unwrap().size, info.size);
    let replacement = reader.export_entries(info.size).unwrap();
    assert_ne!(replacement.entries(), exported.entries());
    assert_eq!(exported.entries().len(), 632);

    let rule = Rule::new(BaseMatch::default(), Vec::new(), Target::Continue).unwrap();
    config
        .replace(table(Verdict::Drop, vec![rule]), |_, _| Ok(()))
        .unwrap();
    assert_eq!(
        reader.export_entries(info.size).unwrap_err(),
        LinuxError::EAGAIN.into()
    );
    assert_eq!(reader.info().unwrap().size, 784);
}

#[def_test]
fn query_counters_match_export_and_encoding_failure_preserves_state() {
    let rule = Rule::new(
        BaseMatch {
            protocol: Some(Negated::new(0, false)),
            ..Default::default()
        },
        Vec::new(),
        Target::Continue,
    )
    .unwrap();
    let table = table(Verdict::Accept, vec![rule]);
    let mut counters = table.collect_counters();
    counters[0].end = FilterCounter {
        packets: 7,
        bytes: 280,
    };
    table.add_counters(&counters).unwrap();
    let config = FilterConfig::new(table);
    let markers = Mutex::new(MarkerCounters::default());
    let reader = LegacyTableHandle {
        config: &config,
        markers: &markers,
    };
    let before = config.with_state(|state| (state.generation(), state.collect_counters()));
    assert_eq!(reader.info().unwrap().size, 784);
    assert_eq!(
        reader.export_entries(784).unwrap_err(),
        LinuxError::EOPNOTSUPP.into()
    );
    assert_eq!(
        config.with_state(|state| (state.generation(), state.collect_counters())),
        before
    );

    let table = FilterTable::accept_all();
    let mut counters = table.collect_counters();
    counters[0].end = FilterCounter {
        packets: 7,
        bytes: 280,
    };
    table.add_counters(&counters).unwrap();
    let expected = LegacyFilterTable::from_table(&table).unwrap();
    config.replace(table, |_, _| Ok(())).unwrap();
    let entries = reader.export_entries(632).unwrap();
    let counter = IptEntry::read_from_prefix(entries.entries())
        .unwrap()
        .0
        .counters;
    assert_eq!((counter.packets, counter.bytes), (7, 280));
    assert_eq!(entries, expected);
}

#[def_test]
fn table_lookup_and_revision_queries_use_fixed_width_c_names() {
    let mut name = [0; TABLE_NAME_LEN];
    name[..6].copy_from_slice(b"filter");
    name[7..].fill(0xff);
    let reader = LegacyTableHandle::lookup(&name).unwrap();
    let info = reader.info().unwrap();
    assert!(reader.export_entries(info.size).is_ok());
    assert_eq!(
        LegacyTableHandle::lookup(&[b'x'; TABLE_NAME_LEN]).err(),
        Some(LinuxError::ENOENT.into())
    );
    for (kind, names) in [
        (ExtensionKind::Match, &["tcp", "udp", "icmp"][..]),
        (ExtensionKind::Target, &["", "ERROR"][..]),
    ] {
        for value in names {
            let mut name = [0; EXTENSION_NAME_LEN];
            name[..value.len()].copy_from_slice(value.as_bytes());
            name[value.len() + 1..].fill(0xff);
            assert_eq!(kind.check_revision(&name, 0), Ok(()));
            assert_eq!(
                kind.check_revision(&name, 1),
                Err(LinuxError::EPROTONOSUPPORT.into())
            );
        }
        assert_eq!(
            kind.check_revision(&[b'x'; EXTENSION_NAME_LEN], 0),
            Err(LinuxError::ENOENT.into())
        );
    }
    assert_eq!(
        ExtensionKind::Match.check_revision(&[0; EXTENSION_NAME_LEN], 0),
        Err(LinuxError::ENOENT.into())
    );
}
