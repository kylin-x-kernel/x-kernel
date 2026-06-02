// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-device descriptors managed by the unified driver pipeline.

#[cfg(feature = "ahci")]
mod ahci;
#[cfg(feature = "bcm2835-sdhci")]
mod bcm2835_sdhci;
#[cfg(feature = "ramdisk")]
mod ramdisk;
#[cfg(feature = "sdmmc")]
mod sdmmc;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "ramdisk")]
    ramdisk::descriptor,
    #[cfg(feature = "ahci")]
    ahci::descriptor,
    #[cfg(feature = "bcm2835-sdhci")]
    bcm2835_sdhci::descriptor,
    #[cfg(feature = "sdmmc")]
    sdmmc::descriptor,
];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
