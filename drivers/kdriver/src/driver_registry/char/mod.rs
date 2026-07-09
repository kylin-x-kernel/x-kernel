// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Character-device descriptors managed by the unified driver pipeline.

#[cfg(any(
    feature = "serial-pl011",
    feature = "serial-ns16550-mmio",
    feature = "serial-ns16550-ioport"
))]
mod serial;

const DRIVER_FACTORIES: &[crate::driver_registry::DriverFactory] = &[
    #[cfg(feature = "serial-pl011")]
    serial::pl011_descriptor,
    #[cfg(feature = "serial-ns16550-mmio")]
    serial::ns16550_mmio_descriptor,
    #[cfg(feature = "serial-ns16550-ioport")]
    serial::ns16550_ioport_descriptor,
];

pub fn register_all(registrar: &mut crate::driver_registry::DriverRegistrar) {
    crate::driver_registry::register_factories(registrar, DRIVER_FACTORIES);
}
