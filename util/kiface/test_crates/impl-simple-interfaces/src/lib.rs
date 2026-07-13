// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

//! Simple interface providers used by `kiface` integration tests.

use define_simple_interfaces::{AdvancedIf, CallerIf, NamespacedIf, SimpleIf};
use kiface::provide;

pub fn force_link() {}

#[provide]
impl SimpleIf {
    fn get_value() -> usize {
        12345
    }

    fn compute(lhs: usize, rhs: usize) -> usize {
        lhs * rhs + 10
    }

    fn get_name() -> &'static str {
        "SimpleImpl"
    }
}

#[provide(namespace = SimpleNs)]
impl NamespacedIf {
    fn get_status() -> bool {
        true
    }

    fn process(value: usize) -> usize {
        value * 2
    }
}

#[provide]
impl CallerIf {
    fn ping() -> usize {
        99
    }

    fn echo(value: usize) -> usize {
        value
    }
}

#[provide(namespace = AdvancedNs)]
impl AdvancedIf {
    fn combine(lhs: usize, rhs: usize) -> usize {
        lhs + rhs
    }

    fn is_ready() -> bool {
        true
    }
}
