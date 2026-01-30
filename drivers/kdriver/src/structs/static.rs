//! Static device type aliases for build-time device selection.
#[cfg(feature = "block")]
pub use crate::drivers::BlockDevice;
#[cfg(feature = "display")]
pub use crate::drivers::DisplayDevice;
#[cfg(feature = "input")]
pub use crate::drivers::InputDevice;
#[cfg(feature = "net")]
pub use crate::drivers::NetDevice;
#[cfg(feature = "vsock")]
pub use crate::drivers::VsockDevice;

impl super::DeviceEnum {
    /// Constructs a network device.
    #[cfg(feature = "net")]
    pub const fn from_net(dev: NetDevice) -> Self {
        Self::Net(dev)
    }

    /// Constructs a block device.
    #[cfg(feature = "block")]
    pub const fn from_block(dev: BlockDevice) -> Self {
        Self::Block(dev)
    }

    /// Constructs a display device.
    #[cfg(feature = "display")]
    pub const fn from_display(dev: DisplayDevice) -> Self {
        Self::Display(dev)
    }

    /// Constructs a display device.
    #[cfg(feature = "input")]
    pub const fn from_input(dev: InputDevice) -> Self {
        Self::Input(dev)
    }

    /// Constructs a vsock device.
    #[cfg(feature = "vsock")]
    pub const fn from_vsock(dev: VsockDevice) -> Self {
        Self::Vsock(dev)
    }
}
