// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Legacy no-firmware PLIC platform layout.
//!
//! This is an explicit platform convention, not a firmware fact. It supplies
//! raw context declarations to the same controller validation used by the
//! firmware path.

extern crate alloc;

use alloc::vec::Vec;

use super::context::PlicContextDecl;

/// Builds the historical `raw_hart_id * 2 + 1` context declarations.
///
/// Only the absence of a PLIC firmware node selects this layout. The result is
/// still validated against the register model and the runtime CPU set by
/// `PlicContexts::build`.
pub(super) fn declarations() -> Result<Vec<PlicContextDecl>, &'static str> {
    let mut declarations = Vec::new();
    let mut failure = None;
    kcpu_id_map::for_each_present_logical_cpu(|_, logical_cpu, _| {
        if failure.is_some() {
            return;
        }
        let Some(raw_cpu_id) = kcpu_id_map::raw_cpu_id(logical_cpu) else {
            failure = Some("missing raw CPU id for legacy PLIC context");
            return;
        };
        let Some(index) = raw_cpu_id
            .as_usize()
            .checked_mul(2)
            .and_then(|value| value.checked_add(1))
        else {
            failure = Some("legacy PLIC context index overflow");
            return;
        };
        let Ok(hardware_cpu_id) = u64::try_from(raw_cpu_id.as_usize()) else {
            failure = Some("legacy PLIC hardware CPU id overflow");
            return;
        };
        declarations.push(PlicContextDecl {
            index,
            hardware_cpu_id,
        });
    });
    failure.map_or(Ok(declarations), Err)
}
