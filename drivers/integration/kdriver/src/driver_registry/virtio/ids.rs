// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared mapping for the VirtIO PCI device ids currently supported by
//! kdriver.
//!
//! The PCI bus backend turns a supported VirtIO PCI device id into a VirtIO
//! type code carried by `PciIdentity::virtio_type`; the matching driver
//! descriptors then key off the same type codes. Both sides reference the
//! constants defined here so supported VirtIO PCI mappings are updated in
//! exactly one place.

use driver_base::DeviceKind;

/// VirtIO device type codes (see VirtIO spec § 4.1.2).
pub mod virtio_type {
    pub const NET: u32 = 1;
    pub const BLOCK: u32 = 2;
    pub const RNG: u32 = 4;
    pub const NINEP: u32 = 9;
    pub const GPU: u32 = 16;
    pub const INPUT: u32 = 18;
    pub const VSOCK: u32 = 19;
}

/// Translate a Red Hat / VirtIO PCI device id (vendor `0x1af4`) into the
/// abstract VirtIO device type code used by the drivers currently wired into
/// kdriver, or `None` if the device id is unknown or not yet supported.
///
/// Supports both the legacy and modern PCI ids for the mapped device types
/// below; it does not claim to cover the full VirtIO PCI id space.
pub fn pci_device_id_to_virtio_type(device_id: u16) -> Option<u32> {
    match device_id {
        0x1000 | 0x1041 => Some(virtio_type::NET),
        0x1001 | 0x1042 => Some(virtio_type::BLOCK),
        0x1005 | 0x1044 => Some(virtio_type::RNG),
        0x1009 | 0x1049 => Some(virtio_type::NINEP),
        0x1050 => Some(virtio_type::GPU),
        0x1052 => Some(virtio_type::INPUT),
        0x1053 => Some(virtio_type::VSOCK),
        _ => None,
    }
}

/// Translate the driver-framework device kind reported by a VirtIO transport
/// probe into the VirtIO device type code used by descriptor matching.
pub const fn device_kind_to_virtio_type(kind: DeviceKind) -> Option<u32> {
    match kind {
        DeviceKind::Net => Some(virtio_type::NET),
        DeviceKind::Block => Some(virtio_type::BLOCK),
        DeviceKind::Char => Some(virtio_type::RNG),
        DeviceKind::Display => Some(virtio_type::GPU),
        DeviceKind::Input => Some(virtio_type::INPUT),
        DeviceKind::Vsock => Some(virtio_type::VSOCK),
        DeviceKind::Fs9p => Some(virtio_type::NINEP),
        DeviceKind::Bus => None,
    }
}

/// VirtIO PCI vendor id assigned to Red Hat.
pub const VIRTIO_PCI_VENDOR_ID: u16 = 0x1af4;
