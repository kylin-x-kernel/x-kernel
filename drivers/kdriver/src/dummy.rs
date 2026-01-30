// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dummy types used if no device of a certain category is selected.

#![allow(unused_imports)]
#![allow(dead_code)]

use cfg_if::cfg_if;

use super::prelude::*;

cfg_if! {
    if #[cfg(net_dev = "dummy")] {
        use net::{MacAddress, NetBuf, NetBufBox, NetBufPool, NetBufHandle};

        /// Placeholder network device.
        pub struct DummyNetDev;
        /// Placeholder network driver.
        pub struct DummyNetDrvier;
        register_net_driver!(DummyNetDriver, DummyNetDev);

        impl DriverOps for DummyNetDev {
            fn device_kind(&self) -> DeviceKind { DeviceKind::Net }
            fn name(&self) -> &str { "dummy-net" }
        }

        impl NetDriverOps for DummyNetDev {
            fn mac(&self) -> MacAddress { unreachable!() }
            fn can_tx(&self) -> bool { false }
            fn can_rx(&self) -> bool { false }
            fn rx_queue_len(&self) -> usize { 0 }
            fn tx_queue_len(&self) -> usize { 0 }
            fn recycle_rx(&mut self, _: NetBufHandle) -> DriverResult { Err(DriverError::Unsupported) }
            fn recycle_tx(&mut self) -> DriverResult { Err(DriverError::Unsupported) }
            fn send(&mut self, _: NetBufHandle) -> DriverResult { Err(DriverError::Unsupported) }
            fn recv(&mut self) -> DriverResult<NetBufHandle> { Err(DriverError::Unsupported) }
            fn alloc_tx_buf(&mut self, _: usize) -> DriverResult<NetBufHandle> { Err(DriverError::Unsupported) }
        }
    }
}

cfg_if! {
    if #[cfg(block_dev = "dummy")] {
        /// Placeholder block device.
        pub struct DummyBlockDev;
        /// Placeholder block driver.
        pub struct DummyBlockDriver;
        register_block_driver!(DummyBlockDriver, DummyBlockDev);

        impl DriverOps for DummyBlockDev {
            fn device_kind(&self) -> DeviceKind {
                DeviceKind::Block
            }
            fn name(&self) -> &str {
                "dummy-block"
            }
        }

        impl BlockDriverOps for DummyBlockDev {
            fn num_blocks(&self) -> u64 {
                0
            }
            fn block_size(&self) -> usize {
                0
            }
            fn read_block(&mut self, _: u64, _: &mut [u8]) -> DriverResult {
                Err(DriverError::Unsupported)
            }
            fn write_block(&mut self, _: u64, _: &[u8]) -> DriverResult {
                Err(DriverError::Unsupported)
            }
            fn flush(&mut self) -> DriverResult {
                Err(DriverError::Unsupported)
            }
        }
    }
}

cfg_if! {
    if #[cfg(display_dev = "dummy")] {
        /// Placeholder display device.
        pub struct DummyDisplayDev;
        /// Placeholder display driver.
        pub struct DummyDisplayDriver;
        register_display_driver!(DummyDisplayDriver, DummyDisplayDev);

        impl DriverOps for DummyDisplayDev {
            fn device_kind(&self) -> DeviceKind {
                DeviceKind::Display
            }
            fn name(&self) -> &str {
                "dummy-display"
            }
        }

        impl DisplayDriverOps for DummyDisplayDev {
            fn info(&self) -> display::DisplayInfo {
                unreachable!()
            }
            fn fb(&self) -> display::FrameBuffer<'_> {
                unreachable!()
            }
            fn need_flush(&self) -> bool {
                false
            }
            fn flush(&mut self) -> DriverResult {
                Err(DriverError::Unsupported)
            }
        }
    }
}

cfg_if! {
    if #[cfg(input_dev = "dummy")] {
        /// Placeholder input device.
        pub struct DummyInputDev;
        /// Placeholder input driver.
        pub struct DummyInputDriver;
        register_input_driver!(DummyInputDriver, DummyInputDev);

        impl DriverOps for DummyInputDev {
            fn device_kind(&self) -> DeviceKind {
                DeviceKind::Input
            }
            fn name(&self) -> &str {
                "dummy-input"
            }
        }

        impl InputDriverOps for DummyInputDev {
            fn device_id(&self) -> InputDeviceId {
                InputDeviceId { bus_type: 0, vendor: 0, product: 0, version: 0 }
            }
            fn physical_location(&self) -> &str {
                "dummy"
            }
            fn unique_id(&self) -> &str {
                "dummy"
            }
            fn get_event_bits(&mut self, _ty: EventType, _out: &mut [u8]) -> DriverResult<bool> {
                Err(DriverError::Unsupported)
            }
            fn read_event(&mut self) -> DriverResult<Event> {
                Err(DriverError::Unsupported)
            }
        }
    }
}

cfg_if! {
    if #[cfg(vsock_dev = "dummy")] {
        /// Placeholder vsock device.
        pub struct DummyVsockDev;
        /// Placeholder vsock driver.
        pub struct DummyVsockDriver;
        register_vsock_driver!(DummyVsockDriver, DummyVsockDev);

        impl DriverOps for DummyVsockDev {
            fn device_kind(&self) -> DeviceKind {
                DeviceKind::Vsock
            }
            fn name(&self) -> &str {
                "dummy-vsock"
            }
        }

        impl VsockDriverOps for DummyVsockDev {
            fn guest_cid(&self) -> u64 {
                unimplemented!()
            }
            fn listen(&mut self, _src_port: u32) {
                unimplemented!()
            }
            fn connect(&mut self, _cid: VsockConnId) -> DriverResult<()> {
                Err(DriverError::Unsupported)
            }
            fn send(&mut self, _cid: VsockConnId, _buf: &[u8]) -> DriverResult<usize> {
                Err(DriverError::Unsupported)
            }
            fn recv(&mut self, _cid: VsockConnId, _buf: &mut [u8]) -> DriverResult<usize> {
                Err(DriverError::Unsupported)
            }
            fn recv_avail(&mut self, _cid: VsockConnId) -> DriverResult<usize> {
                Err(DriverError::Unsupported)
            }
            fn disconnect(&mut self, _cid: VsockConnId) -> DriverResult<()> {
                Err(DriverError::Unsupported)
            }
            fn abort(&mut self, _cid: VsockConnId) -> DriverResult<()> {
                Err(DriverError::Unsupported)
            }
            fn poll_event(&mut self, _buf: &mut [u8]) -> DriverResult<Option<VsockDriverEventType>> {
                Err(DriverError::Unsupported)
            }
        }
    }
}
