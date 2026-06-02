// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Character-device descriptors managed by the unified driver pipeline.

#[cfg(feature = "console")]
mod console;

#[cfg(feature = "console")]
pub(crate) use console::adopt_boot_console;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "console")]
    console::descriptor,
];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
