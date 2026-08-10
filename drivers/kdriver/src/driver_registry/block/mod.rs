// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Block-device descriptors managed by the unified driver pipeline.

#[cfg(any(feature = "ahci", feature = "bcm2835-sdhci", feature = "sdmmc"))]
use core::sync::atomic::{AtomicU32, Ordering};

#[cfg(feature = "ahci")]
static NEXT_SCSI_DISK: AtomicU32 = AtomicU32::new(0);
#[cfg(any(feature = "bcm2835-sdhci", feature = "sdmmc"))]
static NEXT_MMC_DISK: AtomicU32 = AtomicU32::new(0);

#[cfg(feature = "ahci")]
fn allocate_scsi_disk() -> driver_base::DriverResult<(u32, u32)> {
    allocate_disk(&NEXT_SCSI_DISK, 16)
}

#[cfg(any(feature = "bcm2835-sdhci", feature = "sdmmc"))]
fn allocate_mmc_disk() -> driver_base::DriverResult<(u32, u32)> {
    allocate_disk(&NEXT_MMC_DISK, 8)
}

#[cfg(any(feature = "ahci", feature = "bcm2835-sdhci", feature = "sdmmc"))]
fn allocate_disk(counter: &AtomicU32, minors: u32) -> driver_base::DriverResult<(u32, u32)> {
    let index = counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |index| {
            index.checked_mul(minors)?;
            index.checked_add(1)
        })
        .map_err(|_| driver_base::DriverError::InvalidInput)?;
    let first_minor = index * minors;
    Ok((index, first_minor))
}

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
