// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{CpuIdMap, RawCpuId, cpu_id_map_mut_ptr, cpu_map_initialized};

#[inline]
pub const fn normalize_raw_id(raw_cpu_id: RawCpuId) -> RawCpuId {
    raw_cpu_id
}

fn load_raw_cpu_ids_from_fdt(fdt: &of::LinuxFdt<'_>) {
    unsafe {
        CpuIdMap::from_fdt(cpu_id_map_mut_ptr(), fdt, normalize_raw_id);
    }
}

pub(crate) fn ensure_runtime_cpu_id_map() {
    if cpu_map_initialized() {
        return;
    }

    let Some(fdt) = of::fdt() else {
        return;
    };
    load_raw_cpu_ids_from_fdt(fdt);
}

pub fn init_boot_cpu_id_map(dtb_paddr: usize) {
    if cpu_map_initialized() || dtb_paddr == 0 {
        return;
    }

    let dtb_vaddr = kaddr_layout::p2v(dtb_paddr) as *const u8;
    let fdt = unsafe { of::LinuxFdt::from_ptr(dtb_vaddr) }
        .expect("RISC-V boot CPU mapping requires a valid device tree");
    load_raw_cpu_ids_from_fdt(&fdt);
}
