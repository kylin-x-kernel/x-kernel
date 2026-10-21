// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{
    string::{String, ToString},
    vec,
};

use unittest::def_test;

use super::{
    super::{
        matcher::{BaseMatch, ExtensionMatch},
        rule::Rule,
    },
    *,
};

fn push_rule(chain: &mut Chain, rule: Rule) {
    let mut rules = chain.rules().to_vec();
    rules.push(rule);
    *chain = match chain.kind() {
        ChainKind::BuiltIn { hook, policy } => Chain::built_in(*hook, *policy, rules),
        ChainKind::UserDefined { name } => user_chain(name.clone(), rules),
    };
}

fn built_in_chains() -> Vec<Chain> {
    vec![
        Chain::built_in(FilterHook::LocalIn, Verdict::Accept, Vec::new()),
        Chain::built_in(FilterHook::Forward, Verdict::Drop, Vec::new()),
        Chain::built_in(FilterHook::LocalOut, Verdict::Accept, Vec::new()),
    ]
}

fn rule(base: BaseMatch, extensions: Vec<ExtensionMatch>, target: Target) -> Rule {
    Rule::new(base, extensions, target).expect("test rule must be valid")
}

fn user_chain(name: String, rules: Vec<Rule>) -> Chain {
    Chain::user_defined(name, rules).expect("test chain must be valid")
}

#[def_test]
fn filter_table_validates_builtin_hooks() {
    let default_table = FilterTable::accept_all();
    assert!(default_table.chains().iter().all(|chain| matches!(
        chain.kind(),
        ChainKind::BuiltIn {
            policy: Verdict::Accept,
            ..
        }
    )));
    let mut chains = built_in_chains();
    chains.rotate_left(1);
    let table = FilterTable::new(chains).unwrap();
    for (hook, name) in [
        (FilterHook::LocalIn, "INPUT"),
        (FilterHook::Forward, "FORWARD"),
        (FilterHook::LocalOut, "OUTPUT"),
    ] {
        let chain = table.chain(table.built_in_chain_id(hook)).unwrap();
        assert_eq!(chain.name(), name);
    }

    for missing_hook in [
        FilterHook::LocalIn,
        FilterHook::Forward,
        FilterHook::LocalOut,
    ] {
        let mut chains = built_in_chains();
        chains.retain(|chain| {
            !matches!(chain.kind(), ChainKind::BuiltIn { hook, .. } if *hook == missing_hook)
        });
        assert_eq!(
            FilterTable::new(chains),
            Err(FilterTableError::MissingHook(missing_hook))
        );
    }

    let mut duplicate_input = built_in_chains();
    duplicate_input.push(Chain::built_in(
        FilterHook::LocalIn,
        Verdict::Accept,
        Vec::new(),
    ));
    assert_eq!(
        FilterTable::new(duplicate_input),
        Err(FilterTableError::DuplicateHook(FilterHook::LocalIn))
    );

    let mut duplicate_name = built_in_chains();
    duplicate_name.push(user_chain("CUSTOM".to_string(), Vec::new()));
    duplicate_name.push(user_chain("CUSTOM".to_string(), Vec::new()));
    assert_eq!(
        FilterTable::new(duplicate_name),
        Err(FilterTableError::DuplicateChainName)
    );
    let mut reserved_name = built_in_chains();
    reserved_name.push(user_chain("INPUT".to_string(), Vec::new()));
    assert_eq!(
        FilterTable::new(reserved_name),
        Err(FilterTableError::DuplicateChainName)
    );
}

#[def_test]
fn filter_table_enforces_structural_limits() {
    let mut chains = built_in_chains();
    for index in BUILTIN_CHAIN_COUNT..MAX_CHAINS {
        chains.push(user_chain(alloc::format!("CHAIN-{index}"), Vec::new()));
    }
    assert!(FilterTable::new(chains.clone()).is_ok());
    chains.push(user_chain("EXTRA".to_string(), Vec::new()));
    assert_eq!(
        FilterTable::new(chains),
        Err(FilterTableError::TooManyChains)
    );

    let mut chains = built_in_chains();
    chains[0] = Chain::built_in(
        FilterHook::LocalIn,
        Verdict::Accept,
        vec![rule(BaseMatch::default(), Vec::new(), Target::Continue); MAX_RULES - 1],
    );
    push_rule(
        &mut chains[1],
        rule(BaseMatch::default(), Vec::new(), Target::Return),
    );
    assert!(FilterTable::new(chains.clone()).is_ok());
    push_rule(
        &mut chains[2],
        rule(BaseMatch::default(), Vec::new(), Target::Continue),
    );
    assert_eq!(
        FilterTable::new(chains),
        Err(FilterTableError::TooManyRules)
    );
}

#[def_test]
fn filter_table_rejects_invalid_jump_targets() {
    for (target, error) in [
        (
            ChainId::from_index(3),
            FilterTableError::InvalidJumpTarget(ChainId::from_index(3)),
        ),
        (
            ChainId::from_index(1),
            FilterTableError::JumpToBuiltInChain(ChainId::from_index(1)),
        ),
    ] {
        let mut chains = built_in_chains();
        push_rule(
            &mut chains[0],
            rule(BaseMatch::default(), Vec::new(), Target::Jump(target)),
        );
        assert_eq!(FilterTable::new(chains), Err(error));
    }
}

#[def_test]
fn filter_table_rejects_recursive_user_chains() {
    let mut chains = built_in_chains();
    chains.push(user_chain(
        "A".to_string(),
        vec![rule(
            BaseMatch::default(),
            Vec::new(),
            Target::Jump(ChainId::from_index(4)),
        )],
    ));
    chains.push(user_chain(
        "B".to_string(),
        vec![rule(
            BaseMatch::default(),
            Vec::new(),
            Target::Jump(ChainId::from_index(3)),
        )],
    ));

    assert_eq!(FilterTable::new(chains), Err(FilterTableError::ChainCycle));
}

#[def_test]
fn filter_table_accepts_deep_acyclic_jump_graph() {
    let mut chains = built_in_chains();
    push_rule(
        &mut chains[0],
        rule(
            BaseMatch::default(),
            Vec::new(),
            Target::Jump(ChainId::from_index(BUILTIN_CHAIN_COUNT)),
        ),
    );

    for index in BUILTIN_CHAIN_COUNT..MAX_CHAINS {
        let target = if index + 1 < MAX_CHAINS {
            Target::Jump(ChainId::from_index(index + 1))
        } else {
            Target::Return
        };
        chains.push(user_chain(
            alloc::format!("CHAIN-{index}"),
            vec![rule(BaseMatch::default(), Vec::new(), target)],
        ));
    }
    assert!(FilterTable::new(chains).is_ok());
}

#[def_test]
fn filter_table_accepts_shared_chain_at_different_depths() {
    let short_path = ChainId::from_index(BUILTIN_CHAIN_COUNT);
    let long_path = ChainId::from_index(BUILTIN_CHAIN_COUNT + 1);

    for jump_depth in [8, 9] {
        for targets in [[short_path, long_path], [long_path, short_path]] {
            let mut chains = built_in_chains();
            for target in targets {
                push_rule(
                    &mut chains[0],
                    rule(BaseMatch::default(), Vec::new(), Target::Jump(target)),
                );
            }

            let shared_chain = ChainId::from_index(BUILTIN_CHAIN_COUNT + jump_depth);
            chains.push(user_chain(
                "SHORT".to_string(),
                vec![rule(
                    BaseMatch::default(),
                    Vec::new(),
                    Target::Jump(shared_chain),
                )],
            ));
            for depth in 0..jump_depth - 1 {
                let target = ChainId::from_index(BUILTIN_CHAIN_COUNT + depth + 2);
                chains.push(user_chain(
                    alloc::format!("LONG-{depth}"),
                    vec![rule(BaseMatch::default(), Vec::new(), Target::Jump(target))],
                ));
            }
            chains.push(user_chain("SHARED".to_string(), Vec::new()));

            assert!(FilterTable::new(chains).is_ok());
        }
    }
}
