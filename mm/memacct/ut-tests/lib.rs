// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn child_creation_links_parent_and_allocates_unique_ids() {
    let first = create_child(&root());
    let second = create_child(&root());

    assert!(Arc::ptr_eq(first.parent().unwrap(), &root()));
    assert_ne!(first.id(), second.id());
    assert_eq!(get(first.id()).unwrap().id(), first.id());
    assert!(get(MemcgId::from_raw(u32::MAX)).is_none());
}

#[def_test]
fn limit_failure_rolls_back_the_whole_chain_without_double_counting_events() {
    let parent = create_child(&root());
    let child = create_child(&parent);
    child.set_max(Some(4096));

    assert_eq!(charge_bytes(&child, 8192), Err(KError::NoMemory));

    assert_eq!(child.usage(), 0);
    assert_eq!(child.hierarchical_usage(), 0);
    assert_eq!(parent.usage(), 0);
    assert_eq!(parent.hierarchical_usage(), 0);
    assert_eq!(child.events().max, 1);
    assert_eq!(child.events().oom, 0);
    assert_eq!(child.hierarchical_events().max, 1);
    assert_eq!(parent.hierarchical_events().max, 1);
    assert_eq!(parent.events().max, 0);
}

#[def_test]
fn thresholds_do_not_manufacture_reclaim_events() {
    let node = create_child(&root());
    node.set_low(4096);
    node.set_high(Some(8192));

    charge_bytes(&node, 8192).unwrap();
    assert_eq!(node.events().low, 0);
    assert_eq!(node.events().high, 0);
    assert_eq!(node.events().max, 0);
    uncharge_bytes(&node, 8192);
}

#[def_test]
fn hierarchical_counters_include_descendant_charges() {
    let parent = create_child(&root());
    let child = create_child(&parent);

    charge_bytes(&child, 8192).unwrap();

    assert_eq!(child.usage(), 8192);
    assert_eq!(parent.usage(), 0);
    assert_eq!(parent.hierarchical_usage(), 8192);
    assert!(root().hierarchical_usage() >= 8192);
    assert!(parent.hierarchical_peak() >= 8192);

    uncharge_bytes(&child, 8192);
    assert_eq!(parent.hierarchical_usage(), 0);
}

#[def_test]
fn removed_nodes_stay_visible_until_the_last_page_charge_is_released() {
    let node = create_child(&root());
    let id = node.id();
    charge_bytes(&node, 4096).unwrap();

    remove(id);
    assert!(get(id).is_some(), "page charges keep the accounting node alive");
    uncharge_bytes(&node, 4096);
    assert!(get(id).is_none(), "the last release reclaims the removed node");
}

#[def_test]
fn anon_and_file_counters_follow_the_charge_that_created_them() {
    let parent = create_child(&root());
    let child = create_child(&parent);

    charge_bytes_class(&child, 4096, ChargeClass::Anonymous).unwrap();
    charge_bytes_class(&child, 8192, ChargeClass::File).unwrap();

    assert_eq!(
        child.stats(),
        MemoryStats {
            anon: 4096,
            file: 8192,
        }
    );
    assert_eq!(parent.stats(), MemoryStats::default());
    assert_eq!(
        parent.hierarchical_stats(),
        MemoryStats {
            anon: 4096,
            file: 8192,
        }
    );

    uncharge_bytes_class(&child, 4096, ChargeClass::Anonymous);
    uncharge_bytes_class(&child, 8192, ChargeClass::File);
    assert_eq!(child.stats(), MemoryStats::default());
    assert_eq!(parent.hierarchical_stats(), MemoryStats::default());
}

#[def_test]
fn failed_charge_and_peak_reset_keep_visible_counters_coherent() {
    let node = create_child(&root());
    node.set_max(Some(4096));

    assert_eq!(
        charge_bytes_class(&node, 8192, ChargeClass::File),
        Err(KError::NoMemory)
    );
    assert_eq!(node.stats(), MemoryStats::default());
    assert_eq!(node.hierarchical_stats(), MemoryStats::default());

    node.set_max(None);
    charge_bytes_class(&node, 8192, ChargeClass::File).unwrap();
    uncharge_bytes_class(&node, 4096, ChargeClass::File);
    assert_eq!(node.hierarchical_peak(), 8192);

    node.reset_peak();
    assert_eq!(node.peak(), 4096);
    assert_eq!(node.hierarchical_peak(), 4096);
}
