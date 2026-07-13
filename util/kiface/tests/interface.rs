// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kiface::{interface, provide};

#[interface]
trait MathInterface {
    fn add(lhs: usize, rhs: usize) -> usize;
}

#[provide]
impl MathInterface {
    fn add(lhs: usize, rhs: usize) -> usize {
        lhs + rhs
    }
}

#[test]
fn interface_calls_provider() {
    assert_eq!(MathInterface::add(20, 22), 42);
}

#[interface(namespace = test_ns)]
trait NamespacedInterface {
    fn value() -> usize;
}

#[provide(namespace = test_ns)]
impl NamespacedInterface {
    fn value() -> usize {
        7
    }
}

#[test]
fn interface_supports_namespaces() {
    assert_eq!(NamespacedInterface::value(), 7);
}
