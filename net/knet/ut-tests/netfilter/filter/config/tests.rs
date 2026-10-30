// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec};

use unittest::def_test;

use super::*;
use crate::{
    buf::PacketOwner,
    ip::Ipv4Address,
    ipv4::{self, PROTOCOL_ICMP},
    netfilter::filter::{
        chain::{Chain, ChainId},
        counter::FilterCounter,
        matcher::BaseMatch,
        rule::{Rule, Target},
    },
};

fn packet() -> PacketBuf {
    let mut bytes = ipv4::build_ipv4_packet(
        Ipv4Address::new(192, 0, 2, 1),
        Ipv4Address::new(192, 0, 2, 2),
        PROTOCOL_ICMP,
        64,
        &[8, 0, 0, 0, 0, 0, 0, 0],
    )
    .unwrap();
    bytes.extend_from_slice(&[0; 16]);
    let mut packet = PacketBuf::from_ip_packet_vec(1, bytes, PacketOwner::DeviceRx);
    ipv4::validate_input_packet(&mut packet).unwrap();
    packet
}

fn rule(target: Target) -> Rule {
    Rule::new(BaseMatch::default(), Vec::new(), target).unwrap()
}

fn chains(input: Vec<Rule>, policy: Verdict) -> Vec<Chain> {
    vec![
        Chain::built_in(FilterHook::LocalIn, policy, input),
        Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
    ]
}

fn assert_chain_contents_eq(actual: &[Chain], expected: &[Chain]) {
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.rules().len(), expected.rules().len());
        for (actual, expected) in actual.rules().iter().zip(expected.rules()) {
            assert_eq!(actual.base(), expected.base());
            assert_eq!(actual.extensions(), expected.extensions());
            assert_eq!(actual.target(), expected.target());
        }
    }
}

#[def_test]
fn replacement_transfers_old_state_and_rejects_stale_generation() {
    let store = FilterConfig::new(FilterTable::accept_all());
    let packet = packet();
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &packet, "lo", ""),
        Ok(Verdict::Accept)
    );
    let (before_generation, before_counters) =
        store.with_state(|state| (state.generation(), state.collect_counters()));
    assert_eq!(before_generation, FilterGeneration(0));
    let expected_chains = chains(Vec::new(), Verdict::Drop);
    let table = FilterTable::new(chains(Vec::new(), Verdict::Drop)).unwrap();
    let (old, generation) = store
        .replace(table, |_, current| {
            if current != before_generation {
                return Err(FilterConfigError::Conflict);
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(old.generation(), before_generation);
    assert_eq!(old.collect_counters(), before_counters);
    assert_eq!(
        old.collect_counters()[0].end,
        FilterCounter {
            packets: 1,
            bytes: 28
        }
    );
    assert_eq!(generation, FilterGeneration(1));
    store.with_state(|state| {
        assert_chain_contents_eq(state.table().chains(), &expected_chains);
        assert_eq!(state.collect_counters()[0].end, FilterCounter::default());
    });
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &packet, "lo", ""),
        Ok(Verdict::Drop)
    );
    let current_counters = store.with_state(FilterState::collect_counters);
    let result = store.replace(FilterTable::accept_all(), |_, current_generation| {
        if current_generation != before_generation {
            return Err(FilterConfigError::Conflict);
        }
        Ok(())
    });
    assert_eq!(result.err(), Some(FilterConfigError::Conflict));
    store.with_state(|state| {
        assert_eq!(state.generation(), generation);
        assert_chain_contents_eq(state.table().chains(), &expected_chains);
        assert_eq!(state.collect_counters(), current_counters);
    });
    assert_chain_contents_eq(old.table().chains(), &chains(Vec::new(), Verdict::Accept));
    assert_eq!(old.collect_counters(), before_counters);
    let (old, next_generation) = store
        .replace(FilterTable::accept_all(), |_, _| Ok(()))
        .unwrap();
    assert_eq!(old.generation(), generation);
    assert_eq!(next_generation, FilterGeneration(2));
    assert_eq!(store.with_state(FilterState::generation), next_generation);
}

#[def_test]
fn replacement_rejects_generation_overflow_without_changing_state() {
    let store = FilterConfig::new(FilterTable::accept_all());
    store.current.write().generation = FilterGeneration(u64::MAX - 1);
    let table = FilterTable::new(chains(Vec::new(), Verdict::Drop)).unwrap();
    let (old, generation) = store.replace(table, |_, _| Ok(())).unwrap();
    assert_eq!(old.generation(), FilterGeneration(u64::MAX - 1));
    assert_eq!(generation, FilterGeneration(u64::MAX));
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &packet(), "lo", ""),
        Ok(Verdict::Drop)
    );
    let (before_generation, before_counters) =
        store.with_state(|state| (state.generation(), state.collect_counters()));
    assert_eq!(
        store
            .replace(FilterTable::accept_all(), |_, _| Ok(()))
            .err(),
        Some(FilterConfigError::GenerationExhausted)
    );
    store.with_state(|state| {
        assert_eq!(state.generation(), before_generation);
        assert_chain_contents_eq(state.table().chains(), &chains(Vec::new(), Verdict::Drop));
        assert_eq!(state.collect_counters(), before_counters);
    });
}

#[def_test]
fn counter_updates_check_all_shapes_before_changing_any_counter() {
    let table = FilterTable::new(chains(vec![rule(Target::Continue)], Verdict::Accept)).unwrap();
    let store = FilterConfig::new(table);
    let (generation, before) =
        store.with_state(|state| (state.generation(), state.collect_counters()));
    let mut updates = before.clone();
    updates[0].rules[0].packets = 7;
    updates[1].rules.push(FilterCounter::default());
    assert_eq!(
        store.add_counters(|_| Ok(updates)),
        Err(FilterConfigError::InvalidCounterShape)
    );
    assert_eq!(store.with_state(FilterState::collect_counters), before);
    let mut updates = before;
    updates[0].rules[0] = FilterCounter {
        packets: 7,
        bytes: 42,
    };
    updates[0].end = FilterCounter {
        packets: 3,
        bytes: 84,
    };
    assert_eq!(store.add_counters(|_| Ok(updates.clone())), Ok(generation));
    assert_eq!(store.with_state(FilterState::collect_counters), updates);
}

#[def_test]
fn evaluation_accounts_for_rules_and_chain_ends_using_normalized_length() {
    let mut table = chains(
        vec![
            rule(Target::Jump(ChainId::from_index(3))),
            rule(Target::Return),
            rule(Target::Verdict(Verdict::Drop)),
        ],
        Verdict::Accept,
    );
    table.push(Chain::user_defined(String::from("user"), vec![rule(Target::Continue)]).unwrap());
    let store = FilterConfig::new(FilterTable::new(table).unwrap());
    let packet = packet();
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &packet, "eth0", ""),
        Ok(Verdict::Accept)
    );
    let counters = store.with_state(FilterState::collect_counters);
    let counted = FilterCounter {
        packets: 1,
        bytes: 28,
    };
    assert_eq!(
        counters[0].rules,
        vec![counted, counted, FilterCounter::default()]
    );
    assert_eq!(counters[0].end, counted);
    assert_eq!(counters[3].rules, vec![counted]);
    assert_eq!(counters[3].end, counted);
    assert_eq!(counters[1].end, FilterCounter::default());
    let mut malformed = packet;
    malformed.network_packet_mut().unwrap()[0] = 0;
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &malformed, "eth0", ""),
        Err(Ipv4Error::Malformed)
    );
    assert_eq!(store.with_state(FilterState::collect_counters), counters);
}

#[def_test]
fn concurrent_sampling_and_replacement_preserve_final_totals() {
    let table = FilterTable::new(chains(vec![rule(Target::Continue)], Verdict::Accept)).unwrap();
    let replacement =
        FilterTable::new(chains(vec![rule(Target::Continue)], Verdict::Accept)).unwrap();
    let store = Arc::new(FilterConfig::new(table));
    let mut workers = Vec::new();
    store.with_state(|state| {
        for _ in 0..2 {
            let store = Arc::clone(&store);
            workers.push(ktask::spawn(move || {
                let packet = packet();
                for _ in 0..32 {
                    assert_eq!(
                        store.evaluate_ipv4(FilterHook::LocalIn, &packet, "eth0", ""),
                        Ok(Verdict::Accept)
                    );
                    ktask::yield_now();
                }
            }));
        }
        let counters = state.collect_counters();
        assert_eq!(state.generation(), FilterGeneration(0));
        assert_eq!(counters.len(), state.table().chains().len());
        for (counter, chain) in counters.iter().zip(state.table().chains()) {
            assert_eq!(counter.rules.len(), chain.rules().len());
        }
        // One evaluator must finish while the state remains borrowed.
        workers.pop().unwrap().join();
    });
    let (old, generation) = store.replace(replacement, |_, _| Ok(())).unwrap();
    let old_counters = old.collect_counters();
    assert_eq!(
        store.evaluate_ipv4(FilterHook::LocalIn, &packet(), "eth0", ""),
        Ok(Verdict::Accept)
    );
    for worker in workers {
        worker.join();
    }
    assert_eq!(old.collect_counters(), old_counters);
    let current_counters = store.with_state(|state| {
        assert_eq!(state.generation(), generation);
        state.collect_counters()
    });
    assert_eq!(old_counters[0].rules[0], old_counters[0].end);
    assert_eq!(current_counters[0].rules[0], current_counters[0].end);
    assert_eq!(
        old_counters[0].end.packets + current_counters[0].end.packets,
        65
    );
    assert_eq!(
        old_counters[0].end.bytes + current_counters[0].end.bytes,
        65 * 28
    );
}
