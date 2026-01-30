// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform-specific constants and parameters for X-Kernel.
// NOTE: keep config docs in sync with configs (e.g., optional AHCI/PMU PADDR/IRQ).
//! Currently supported platform configs can be found in the [configs] directory of
//! the [X-Kernel] root.
//!
//! [X-Kernel]: https://github.com/kylin-x-kernel/x-kernel
//! [configs]: https://github.com/kylin-x-kernel/x-kernel/tree/main/configs
#![no_std]

platconfig_macros::include_configs!(
    path_env = "PLAT_CONFIG_PATH",
    fallback = "../../configs/dummy.toml"
);
