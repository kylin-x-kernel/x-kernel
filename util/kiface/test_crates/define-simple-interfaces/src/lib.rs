// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Simple interface definitions used by `kiface` integration tests.
#![no_std]

use kiface::interface;

/// A simple interface with basic associated functions.
#[interface]
pub trait SimpleIf {
    fn get_value() -> usize;
    fn compute(lhs: usize, rhs: usize) -> usize;
    /// Returns the provider name.
    fn get_name() -> &'static str;
}

/// An interface using a symbol namespace.
#[interface(namespace = SimpleNs)]
pub trait NamespacedIf {
    fn get_status() -> bool;
    fn process(value: usize) -> usize;
}

/// An interface whose facade methods are called directly by consumers.
#[interface]
pub trait CallerIf {
    fn ping() -> usize;
    fn echo(value: usize) -> usize;
}

/// An interface using both direct calls and a namespace.
#[interface(namespace = AdvancedNs)]
pub trait AdvancedIf {
    fn combine(lhs: usize, rhs: usize) -> usize;
    fn is_ready() -> bool;
}
