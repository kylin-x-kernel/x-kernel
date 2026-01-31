// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform NMI (or pseudo-NMI) interface.

use kplat_macros::device_interface;

#[derive(Clone, Copy, Debug)]
pub enum NmiType {
    /// True hardware NMI (cannot be masked by IRQ disable)
    TrueNmi,
    /// Pseudo NMI (implemented via high-priority IRQ / FIQ / SGI)
    PseudoNmi,
    /// Not supported
    None,
}

/// NMI handler callback type.
pub type NmiHandler = fn();

#[device_interface]
pub trait NmiDef {
    /// Initializes NMI facilities with a threshold value.
    fn init(thresh: u64) -> bool;
    /// Returns the supported NMI type for the platform.
    fn nmi_type() -> NmiType;
    /// Enables NMI delivery.
    fn enable();
    /// Disables NMI delivery.
    fn disable();
    /// Returns whether NMI delivery is enabled.
    fn is_enabled() -> bool;
    /// Returns the NMI implementation name.
    fn name() -> &'static str;
    /// Registers an NMI handler callback.
    fn register_nmi_handler(cb: NmiHandler) -> bool;
}
