// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serial integration fixtures; never infer production exemptions from names.

use alloc::vec::Vec;

use crate::KtaskRef;

/// Captures admitted tasks for explicit exemptions in serial kernel fixtures.
///
/// The caller must retain fixture ownership and exclude its own target tasks
/// and coordinator. This is a temporary snapshot, not a production PM policy.
///
/// # Panics
/// Panics if another system-freezer snapshot already owns admission.
pub fn admitted_tasks() -> Vec<KtaskRef> {
    let gate = super::system::PublicationGate::acquire().unwrap();
    gate.snapshot()
}
