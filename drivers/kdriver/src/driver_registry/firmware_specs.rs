// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Firmware match specs shared by built-in platform drivers.

#[cfg(any(
    feature = "ahci",
    feature = "bcm2835-sdhci",
    feature = "sdmmc",
    feature = "fxmac"
))]
use kdevice::FirmwareMatchSpec;

/// AHCI SATA host controller.
#[cfg(feature = "ahci")]
pub(crate) const AHCI: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "ahci",
    dt_compatibles: &["generic-ahci", "snps,dwc-ahci"],
    acpi_ids: &[],
};

/// Broadcom BCM2835 SDHCI controller (Raspberry Pi family).
#[cfg(feature = "bcm2835-sdhci")]
pub(crate) const BCM2835_SDHCI: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "bcm2835-sdhci",
    dt_compatibles: &["brcm,bcm2835-sdhci", "brcm,bcm2711-emmc2"],
    acpi_ids: &[],
};

/// Generic SD/MMC host controller.
#[cfg(feature = "sdmmc")]
pub(crate) const SDMMC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "sdmmc",
    dt_compatibles: &["snps,dwcmshc", "arm,pl18x"],
    acpi_ids: &[],
};

/// Phytium / Cadence GEM Ethernet controller.
#[cfg(feature = "fxmac")]
pub(crate) const FXMAC: FirmwareMatchSpec = FirmwareMatchSpec {
    alias: "fxmac",
    dt_compatibles: &["cdns,phytium-gem-1.0", "cdns,gem", "cdns,zynqmp-gem"],
    acpi_ids: &[],
};
