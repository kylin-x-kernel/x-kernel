// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TODO: generate registered drivers in `for_each_drivers!` automatically.

//! Driver registration and enumeration macros.

#![allow(unused_macros)]

/// Define the unified type for network devices.
macro_rules! register_net_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the NIC devices.
        pub type NetDevice = $device_type;
    };
}

/// Define the unified type for block devices.
macro_rules! register_block_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the block devices.
        pub type BlockDevice = $device_type;
    };
}

/// Define the unified type for display devices.
macro_rules! register_display_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the display devices.
        pub type DisplayDevice = $device_type;
    };
}

/// Define the unified type for input devices.
macro_rules! register_input_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the input devices.
        pub type InputDevice = $device_type;
    };
}

/// Define the unified type for vsock devices.
macro_rules! register_vsock_driver {
    ($driver_type:ty, $device_type:ty) => {
        /// The unified type of the vsock devices.
        pub type VsockDevice = $device_type;
    };
}

/// Expand to iterate through all registered drivers under the current build config.
macro_rules! for_each_drivers {
    (type $drv_type:ident, $code:block) => {{
        #[allow(unused_imports)]
        use crate::drivers::DriverProbe;
        #[cfg(feature = "virtio")]
        #[allow(unused_imports)]
        use crate::virtio::{self, VirtIoDevMeta};

        #[cfg(net_dev = "virtio-net")]
        {
            type $drv_type = <virtio::VirtIoNet as VirtIoDevMeta>::Driver;
            $code
        }
        #[cfg(block_dev = "virtio-blk")]
        {
            type $drv_type = <virtio::VirtIoBlk as VirtIoDevMeta>::Driver;
            $code
        }
        #[cfg(display_dev = "virtio-gpu")]
        {
            type $drv_type = <virtio::VirtIoGpu as VirtIoDevMeta>::Driver;
            $code
        }
        #[cfg(input_dev = "virtio-input")]
        {
            type $drv_type = <virtio::VirtIoInput as VirtIoDevMeta>::Driver;
            $code
        }
        #[cfg(vsock_dev = "virtio-socket")]
        {
            type $drv_type = <virtio::VirtIoSocket as VirtIoDevMeta>::Driver;
            $code
        }
        #[cfg(block_dev = "ramdisk")]
        {
            type $drv_type = crate::drivers::RamDiskDriver;
            $code
        }
        #[cfg(block_dev = "sdmmc")]
        {
            type $drv_type = crate::drivers::SdMmcDriver;
            $code
        }
        #[cfg(block_dev = "ahci")]
        {
            type $drv_type = crate::drivers::AhciDriver;
            $code
        }
        #[cfg(block_dev = "bcm2835-sdhci")]
        {
            type $drv_type = crate::drivers::BcmSdhciDriver;
            $code
        }
        #[cfg(net_dev = "ixgbe")]
        {
            type $drv_type = crate::drivers::IxgbeDriver;
            $code
        }
        #[cfg(net_dev = "fxmac")]
        {
            type $drv_type = crate::drivers::FXmacDriver;
            $code
        }
    }};
}
