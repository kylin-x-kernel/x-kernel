// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::cpu_id::{RawCpuId, cpu_map_initialized, load_cpu_id_map_from_madt};

#[inline]
pub const fn normalize_raw_id(raw_cpu_id: RawCpuId) -> RawCpuId {
    raw_cpu_id
}

fn load_raw_cpu_ids_from_madt(entries: acpi::MadtEntryIter) {
    load_cpu_id_map_from_madt(entries, normalize_raw_id);
}

pub(crate) fn ensure_runtime_cpu_id_map() {
    if cpu_map_initialized() {
        return;
    }

    let Some((_, entries)) = acpi::find_madt_from_init() else {
        return;
    };
    load_raw_cpu_ids_from_madt(entries);
}

pub fn init_boot_cpu_id_map(rsdp_addr: usize) {
    if cpu_map_initialized() || rsdp_addr == 0 {
        return;
    }

    let Some((_, entries)) = acpi::find_madt_from_rsdp(rsdp_addr) else {
        return;
    };
    load_raw_cpu_ids_from_madt(entries);
}
