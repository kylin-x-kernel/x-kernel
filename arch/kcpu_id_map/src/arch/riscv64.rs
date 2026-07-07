// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::cpu_id::{RawCpuId, cpu_map_initialized, load_cpu_id_map_from_fdt};

#[inline]
pub const fn normalize_raw_id(raw_cpu_id: RawCpuId) -> RawCpuId {
    raw_cpu_id
}

fn load_raw_cpu_ids_from_fdt(fdt: &of::LinuxFdt<'_>) {
    let is_truncated = load_cpu_id_map_from_fdt(fdt, normalize_raw_id);
    if is_truncated {
        log::warn!(
            "device tree describes more CPUs than NR_CPUS={}; extra CPUs ignored",
            kbuild_config::NR_CPUS
        );
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
    // SAFETY: `dtb_paddr` comes from boot firmware; after `p2v` it points to
    // the readable DTB blob used for CPU enumeration during early boot.
    let fdt = unsafe { of::LinuxFdt::from_ptr(dtb_vaddr) }
        .expect("RISC-V boot CPU mapping requires a valid device tree");
    load_raw_cpu_ids_from_fdt(&fdt);
}
