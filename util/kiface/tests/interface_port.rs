// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cross-crate integration tests for simple interface shapes.

use define_simple_interfaces::{AdvancedIf, CallerIf, NamespacedIf, SimpleIf};

fn link_providers() {
    impl_simple_interfaces::force_link();
}

#[test]
fn simple_interface_calls_cross_crate_provider() {
    link_providers();

    assert_eq!(SimpleIf::get_value(), 12345);
    assert_eq!(SimpleIf::compute(10, 5), 60);
    assert_eq!(SimpleIf::get_name(), "SimpleImpl");
}

#[test]
fn namespaced_interface_calls_cross_crate_provider() {
    link_providers();

    assert!(NamespacedIf::get_status());
    assert_eq!(NamespacedIf::process(42), 84);
}

#[test]
fn direct_facade_calls_replace_generated_callers() {
    link_providers();

    assert_eq!(CallerIf::ping(), 99);
    assert_eq!(CallerIf::echo(123), 123);
}

#[test]
fn advanced_namespaced_interface_calls_cross_crate_provider() {
    link_providers();

    assert_eq!(AdvancedIf::combine(3, 4), 7);
    assert!(AdvancedIf::is_ready());
}

#[test]
fn repeated_interface_calls_remain_stable() {
    link_providers();

    for i in 0..16 {
        assert_eq!(SimpleIf::compute(i, i), i * i + 10);
    }
}
