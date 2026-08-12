// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(fdt) = rs_fdtree::LinuxFdt::new(data) else {
        return;
    };

    for node in fdt.all_nodes() {
        let _ = node.name;
        for prop in node.properties() {
            let _ = prop.name;
        }
    }
});
