// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Resolved build-configuration constants supplied by the configuration generator.
//!
//! `CONFIG_GZIP` contains the same resolved snapshot in deterministic gzip form
//! for runtime configuration introspection. It is generated during the normal
//! Make/Kconfig flow together with the typed constants.
#![no_std]

// Include the generated config.rs file
include!(env!("CONFIG_RS_PATH"));
