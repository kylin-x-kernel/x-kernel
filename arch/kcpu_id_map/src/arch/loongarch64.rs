// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::cpu_id::{RawCpuId, cpu_map_initialized, load_cpu_id_map, load_cpu_id_map_from_fdt};

#[inline]
pub(crate) const fn normalize_raw_id(raw_cpu_id: RawCpuId) -> RawCpuId {
    raw_cpu_id
}

fn load_raw_cpu_ids_from_fdt(fdt: &of::LinuxFdt<'_>, boot_cpu_id: Option<RawCpuId>) {
    let is_truncated = if let Some(boot_cpu_id) = boot_cpu_id {
        let raw_cpu_ids = || {
            of::enabled_cpu_nodes(fdt)
                .filter_map(of::cpu_node_reg)
                .map(|raw_cpu_id| normalize_raw_id(RawCpuId::new(raw_cpu_id as usize)))
        };
        let boot_cpu = raw_cpu_ids().find(|raw_cpu_id| *raw_cpu_id == boot_cpu_id);
        load_cpu_id_map(|map| {
            map.load_from_raw_cpu_ids(
                boot_cpu
                    .into_iter()
                    .chain(raw_cpu_ids().filter(|raw_cpu_id| *raw_cpu_id != boot_cpu_id)),
            )
        })
    } else {
        load_cpu_id_map_from_fdt(fdt, normalize_raw_id)
    };
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
    load_raw_cpu_ids_from_fdt(fdt, None);
}

/// Initializes the LoongArch CPU map, retaining the actual boot CPU before
/// truncating firmware enumeration to `NR_CPUS`. The boot CPU must still be
/// present in an enabled firmware CPU node; no missing entry is synthesized.
pub fn init_boot_cpu_id_map(dtb_paddr: usize, boot_cpu_id: RawCpuId) {
    if cpu_map_initialized() || dtb_paddr == 0 {
        return;
    }

    let dtb_vaddr = kaddr_layout::p2v(dtb_paddr) as *const u8;
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: the firmware CPU-map device \
                  tree is invalid; early CPU initialization has no error return or recovery"
    )]
    // SAFETY: `dtb_paddr` comes from boot firmware; after `p2v` it points to
    // the readable DTB blob used for CPU enumeration during early boot.
    let fdt = unsafe { of::LinuxFdt::from_ptr(dtb_vaddr) }
        .expect("LoongArch boot CPU mapping requires a valid device tree");
    load_raw_cpu_ids_from_fdt(&fdt, Some(boot_cpu_id));
}
