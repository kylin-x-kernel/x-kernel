// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Network-device descriptors managed by the unified driver pipeline.

#[cfg(feature = "fxmac")]
mod fxmac;
#[cfg(feature = "ixgbe")]
mod ixgbe;
#[cfg(feature = "ixgbe")]
mod ixgbe_hal;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "fxmac")]
    fxmac::descriptor,
    #[cfg(feature = "ixgbe")]
    ixgbe::descriptor,
];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
