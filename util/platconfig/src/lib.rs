<<<<<<< HEAD
=======
// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2025-2026 KylinSoft Co., Ltd. https://www.kylinos.cn/
// See LICENSES for license details.
>>>>>>> aed4a31 (refactor: rename axconfig to platconfig)
//! Platform-specific constants and parameters for X-Kernel.
//!
//! Currently supported platform configs can be found in the [configs] directory of
//! the [X-Kernel] root.
//!
//! [X-Kernel]: https://github.com/kylin-x-kernel/x-kernel
//! [configs]: https://github.com/kylin-x-kernel/x-kernel/tree/main/configs
#![no_std]

<<<<<<< HEAD
platconfig_macros::include_configs!(
=======
axconfig_macros::include_configs!(
>>>>>>> aed4a31 (refactor: rename axconfig to platconfig)
    path_env = "PLAT_CONFIG_PATH",
    fallback = "../../configs/dummy.toml"
);
