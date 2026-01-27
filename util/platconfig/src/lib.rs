// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025-2026 KylinSoft Co., Ltd. https://www.kylinos.cn/
// See LICENSES for license details.
//! Platform-specific constants and parameters for X-Kernel.
//!
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
