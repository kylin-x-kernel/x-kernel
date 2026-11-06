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

#[interface(namespace = unsafe_test)]
trait BorrowedValue {
    /// # Safety
    /// `value` must point to a readable, aligned usize for this call.
    unsafe fn read(value: *const usize) -> usize;
}

#[provide(namespace = unsafe_test)]
impl BorrowedValue {
    /// # Safety
    /// The caller supplies a readable aligned usize for this call.
    unsafe fn read(value: *const usize) -> usize {
        // SAFETY: the caller supplies a live, aligned usize.
        unsafe { *value }
    }
}

#[test]
fn unsafe_interface_preserves_signature_and_provider_call() {
    let read: unsafe fn(*const usize) -> usize = BorrowedValue::read;
    let value = 42;
    // SAFETY: value is live and borrowed until the provider returns.
    assert_eq!(unsafe { read(&value) }, value);
}
