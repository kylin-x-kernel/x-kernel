// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple interface definitions used by `kiface` integration tests.
#![no_std]

use kiface::interface;

/// An unsafe contract crossing a crate and symbol namespace boundary.
#[interface(namespace = BorrowedNs)]
pub trait BorrowedIf {
    /// Reads the caller's value.
    ///
    /// # Safety
    /// `value` must point to a readable aligned usize throughout the call.
    unsafe fn read(value: *const usize) -> usize;
}

/// A simple interface with basic associated functions.
#[interface]
pub trait SimpleIf {
    /// Returns a constant test value.
    fn get_value() -> usize;
    /// Combines two inputs with a deterministic arithmetic expression.
    fn compute(lhs: usize, rhs: usize) -> usize;
    /// Returns the provider name.
    fn get_name() -> &'static str;
}

/// An interface using a symbol namespace.
#[interface(namespace = SimpleNs)]
pub trait NamespacedIf {
    /// Returns a fixed readiness flag.
    fn get_status() -> bool;
    /// Transforms the input by a fixed rule.
    fn process(value: usize) -> usize;
}

/// An interface whose facade methods are called directly by consumers.
#[interface]
pub trait CallerIf {
    /// Returns a constant used to verify direct facade calls.
    fn ping() -> usize;
    /// Returns the input unchanged to verify argument passing.
    fn echo(value: usize) -> usize;
}

/// An interface using both direct calls and a namespace.
#[interface(namespace = AdvancedNs)]
pub trait AdvancedIf {
    /// Combines two inputs to verify cross-namespace argument passing.
    fn combine(lhs: usize, rhs: usize) -> usize;
    /// Returns a fixed readiness flag for namespace calls.
    fn is_ready() -> bool;
}
