//! Device driver prelude that includes some traits and types.

pub use driver_base::{DeviceKind, DriverError, DriverOps, DriverResult};
#[cfg(feature = "block")]
pub use {crate::structs::AxBlockDevice, block::BlockDriverOps};
#[cfg(feature = "display")]
pub use {
    crate::structs::AxDisplayDevice,
    display::{DisplayDriverOps, DisplayInfo},
};
#[cfg(feature = "input")]
pub use {
    crate::structs::AxInputDevice,
    input::{Event, EventType, InputDeviceId, InputDriverOps},
};
#[cfg(feature = "net")]
pub use {
    crate::structs::AxNetDevice,
    axdriver_net::{NetBufPtr, NetDriverOps},
};
#[cfg(feature = "vsock")]
pub use {
    crate::structs::AxVsockDevice,
    vsock::{VsockAddr, VsockConnId, VsockDriverEventType, VsockDriverOps},
};
