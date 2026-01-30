// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device driver prelude that includes some traits and types.

pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
#[cfg(feature = "block")]
pub use {crate::structs::BlockDevice, block::BlockDriverOps};
#[cfg(feature = "display")]
pub use {
    crate::structs::DisplayDevice,
    display::{DisplayDriverOps, DisplayInfo},
};
#[cfg(feature = "input")]
pub use {
    crate::structs::InputDevice,
    input::{Event, EventType, InputDeviceId, InputDriverOps},
};
#[cfg(feature = "net")]
pub use {
    crate::structs::NetDevice,
    net::{NetBufHandle, NetDriverOps},
};
#[cfg(feature = "vsock")]
pub use {
    crate::structs::VsockDevice,
    vsock::{VsockAddr, VsockConnId, VsockDriverEventType, VsockDriverOps},
};
