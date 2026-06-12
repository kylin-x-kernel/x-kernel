// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

cfg_select! {
    target_arch = "aarch64" => {
        pub(crate) mod aarch64;
        pub use aarch64::init_boot_cpu_id_map;
        pub(crate) use self::aarch64 as imp;
    }
    target_arch = "loongarch64" => {
        pub(crate) mod loongarch64;
        pub use loongarch64::init_boot_cpu_id_map;
        pub(crate) use self::loongarch64 as imp;
    }
    target_arch = "riscv64" => {
        pub(crate) mod riscv64;
        pub use riscv64::init_boot_cpu_id_map;
        pub(crate) use self::riscv64 as imp;
    }
    target_arch = "x86_64" => {
        pub(crate) mod x86_64;
        pub use x86_64::init_boot_cpu_id_map;
        pub(crate) use self::x86_64 as imp;
    }
}
