// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed runtime device classes built on top of the device core.
//!
//! The generic class-device handle and registry mechanics live in the internal
//! `generic` module. This crate root wires those mechanics to concrete
//! block/net/char/display/input/vsock/9P operation traits.
//! Drivers publish runtime capabilities here after probe succeeds so subsystems
//! can enumerate existing devices and subscribe to later availability without
//! depending on probe order.

#![no_std]

extern crate alloc;

#[cfg(feature = "virtio-9p")]
use alloc::string::String;
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use driver_base::{DeviceKind, DriverError, DriverResult};
use kdevice::{DeviceEvent, DeviceEventKind, DeviceId, DeviceObject};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

mod generic;

pub use self::generic::{ClassAvailabilityCallback, ClassDevice};
use self::generic::{ClassDeviceMetadata, ClassRegistry};

static ACTIVATION_BRIDGE: LazyInit<()> = LazyInit::new();

fn ensure_event_bridge() {
    ACTIVATION_BRIDGE.call_once(|| {
        kdevice::subscribe_device_event_kind(
            DeviceEventKind::Activated,
            Arc::new(|event| {
                let DeviceEvent::Activated { id, kind } = event else {
                    return;
                };
                notify_class_available(kind, id);
            }),
        );
        kdevice::subscribe_device_event_kind(
            DeviceEventKind::Removed,
            Arc::new(|event| {
                let DeviceEvent::Removed { id } = event else {
                    return;
                };
                remove_class_device(id);
            }),
        );
    });
}

macro_rules! class_registry {
    (
        $feature:literal,
        $ty:ty,
        $kind:expr,
        $registry:ident,
        $registry_fn:ident,
        $publish_fn:ident,
        $devices_fn:ident,
        $find_fn:ident,
        $subscribe_fn:ident,
        $notify_fn:ident,
        $remove_fn:ident
    ) => {
        #[cfg(feature = $feature)]
        static $registry: LazyInit<SpinNoPreempt<ClassRegistry<$ty>>> = LazyInit::new();

        #[cfg(feature = $feature)]
        fn $registry_fn() -> &'static SpinNoPreempt<ClassRegistry<$ty>> {
            ensure_event_bridge();
            $registry.call_once(|| SpinNoPreempt::new(ClassRegistry::new()));
            $registry.get().expect("class registry call_once succeeded")
        }

        /// Publish a runtime capability into its typed class registry.
        #[cfg(feature = $feature)]
        pub fn $publish_fn(parent: Arc<DeviceObject>, runtime: $ty) -> DriverResult<()> {
            let name = runtime.name().into();
            let device_kind = runtime.device_kind();
            let irq = runtime.irq();
            if device_kind != $kind {
                log::warn!(
                    "device class '{}': runtime kind {:?} does not match registry kind {:?}",
                    $kind.as_str(),
                    device_kind,
                    $kind,
                );
                return Err(DriverError::InvalidInput);
            }
            let metadata = runtime.class_metadata();
            let device = ClassDevice::try_new_with_class_metadata(
                parent, runtime, name, $kind, irq, metadata,
            )?;
            let id = device.id();
            let is_active = device.is_available();
            log::debug!(
                "device class '{}': published {:?} (driver={})",
                $kind.as_str(),
                id,
                device.driver_name(),
            );
            $registry_fn().lock().publish(device);
            if is_active {
                notify_class_available($kind, id);
            }
            Ok(())
        }

        /// Return every currently available runtime capability for this class.
        #[cfg(feature = $feature)]
        pub fn $devices_fn() -> Vec<ClassDevice<$ty>> {
            $registry_fn().lock().devices()
        }

        /// Find one currently available runtime capability by device ID.
        #[cfg(feature = $feature)]
        pub fn $find_fn(id: DeviceId) -> Option<ClassDevice<$ty>> {
            $registry_fn().lock().find(id)
        }

        /// Subscribe to future runtime capabilities for this class.
        #[cfg(feature = $feature)]
        pub fn $subscribe_fn(callback: ClassAvailabilityCallback<$ty>) {
            $registry_fn().lock().subscribe_available(callback);
        }

        #[cfg(feature = $feature)]
        fn $notify_fn(id: DeviceId) {
            let (device, callbacks) = {
                let registry = $registry_fn().lock();
                let Some(device) = registry.find(id) else {
                    return;
                };
                (device, registry.availability_callbacks())
            };

            for callback in callbacks {
                callback(device.clone());
            }
        }

        #[cfg(feature = $feature)]
        fn $remove_fn(id: DeviceId) {
            $registry_fn().lock().remove(id);
        }
    };
}

macro_rules! class_registries {
    (
        $(
            $feature:literal,
            $ty:ty,
            $kind:path,
            $registry:ident,
            $registry_fn:ident,
            $publish_fn:ident,
            $devices_fn:ident,
            $find_fn:ident,
            $subscribe_fn:ident,
            $notify_fn:ident,
            $remove_fn:ident
        );+ $(;)?
    ) => {
        fn notify_class_available(kind: DeviceKind, id: DeviceId) {
            match kind {
                $(
                    #[cfg(feature = $feature)]
                    $kind => $notify_fn(id),
                )+
                _ => {}
            }
        }

        fn remove_class_device(id: DeviceId) {
            $(
                #[cfg(feature = $feature)]
                $remove_fn(id);
            )+
        }

        $(
            class_registry!(
                $feature,
                $ty,
                $kind,
                $registry,
                $registry_fn,
                $publish_fn,
                $devices_fn,
                $find_fn,
                $subscribe_fn,
                $notify_fn,
                $remove_fn
            );
        )+
    };
}

#[cfg(feature = "net")]
pub type NetDeviceImpl = Box<dyn NetDevice + Send + Sync>;
#[cfg(feature = "block")]
pub type BlockDeviceImpl = Box<dyn BlockDevice + Send + Sync>;
#[cfg(feature = "char")]
pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;
#[cfg(feature = "display")]
pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;
#[cfg(feature = "input")]
pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;
#[cfg(feature = "vsock")]
pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;
#[cfg(feature = "virtio-9p")]
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

#[cfg(feature = "block")]
pub use block::BlockDevice;
#[cfg(feature = "char")]
pub use char_driver::CharDevice;
#[cfg(feature = "display")]
pub use display::{DisplayDevice, DisplayInfo};
#[cfg(feature = "input")]
pub use input::{Event, EventType, InputDevice, InputDeviceId};
#[cfg(feature = "net")]
pub use net::{NetBufHandle, NetDevice};
#[cfg(feature = "virtio-9p")]
pub use virtio::Virtio9pDevice;
#[cfg(feature = "vsock")]
pub use vsock::{VsockAddr, VsockConnId, VsockDevice, VsockDriverEventType};

trait ClassRuntimeMetadata {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::empty()
    }
}

#[cfg(feature = "block")]
impl ClassRuntimeMetadata for BlockDeviceImpl {}

#[cfg(feature = "char")]
impl ClassRuntimeMetadata for CharDeviceImpl {}

#[cfg(feature = "display")]
impl ClassRuntimeMetadata for DisplayDeviceImpl {}

#[cfg(feature = "input")]
impl ClassRuntimeMetadata for InputDeviceImpl {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::input(self.physical_location().into(), self.unique_id().into())
    }
}

#[cfg(feature = "net")]
impl ClassRuntimeMetadata for NetDeviceImpl {}

#[cfg(feature = "vsock")]
impl ClassRuntimeMetadata for VsockDeviceImpl {}

#[cfg(feature = "virtio-9p")]
impl ClassRuntimeMetadata for Virtio9pDeviceImpl {}

#[cfg(feature = "block")]
impl BlockDevice for ClassDevice<BlockDeviceImpl> {
    fn num_blocks(&self) -> u64 {
        self.with(|device| device.num_blocks())
    }

    fn block_size(&self) -> usize {
        self.with(|device| device.block_size())
    }

    fn read_block(&mut self, block_id: u64, buf: &mut [u8]) -> DriverResult {
        self.with_mut(|device| device.read_block(block_id, buf))
    }

    fn write_block(&mut self, block_id: u64, buf: &[u8]) -> DriverResult {
        self.with_mut(|device| device.write_block(block_id, buf))
    }

    fn flush(&mut self) -> DriverResult {
        self.with_mut(|device| device.flush())
    }
}

#[cfg(feature = "char")]
impl CharDevice for ClassDevice<CharDeviceImpl> {
    fn read(&mut self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with_mut(|device| device.read(buf))
    }

    fn write(&mut self, buf: &[u8]) -> DriverResult<usize> {
        self.with_mut(|device| device.write(buf))
    }

    fn flush(&mut self) -> DriverResult {
        self.with_mut(|device| device.flush())
    }
}

#[cfg(feature = "display")]
impl DisplayDevice for ClassDevice<DisplayDeviceImpl> {
    fn info(&self) -> DisplayInfo {
        self.with(|device| device.info())
    }

    fn fb(&self) -> display::FrameBuffer<'_> {
        let info = self.info();
        // SAFETY: The framebuffer mapping `(fb_base_vaddr, fb_size)` is a
        // stable runtime device resource installed by the display driver
        // during probe and torn down only when the device is removed.
        // While the returned `FrameBuffer` is alive the caller holds an
        // `Arc<ClassDeviceInner<DisplayDeviceImpl>>` (via `Self`), so the
        // underlying display device cannot be removed and the mapping is
        // guaranteed to remain valid for `fb_size` bytes starting at
        // `fb_base_vaddr`. The framebuffer is only mutated through
        // `flush()` (which takes `&mut self`), so no other code mutates
        // this region concurrently.
        unsafe {
            display::FrameBuffer::from_raw_parts_mut(info.fb_base_vaddr as *mut u8, info.fb_size)
        }
    }

    fn need_flush(&self) -> bool {
        self.with(|device| device.need_flush())
    }

    fn flush(&mut self) -> DriverResult {
        self.with_mut(|device| device.flush())
    }
}

#[cfg(feature = "input")]
impl InputDevice for ClassDevice<InputDeviceImpl> {
    fn device_id(&self) -> InputDeviceId {
        self.with(|device| device.device_id())
    }

    fn physical_location(&self) -> &str {
        self.input_physical_location()
    }

    fn unique_id(&self) -> &str {
        self.input_unique_id()
    }

    fn get_event_bits(&mut self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.with_mut(|device| device.get_event_bits(ty, out))
    }

    fn read_event(&mut self) -> DriverResult<Event> {
        self.with_mut(|device| device.read_event())
    }
}

#[cfg(feature = "net")]
impl NetDevice for ClassDevice<NetDeviceImpl> {
    fn mac(&self) -> net::MacAddress {
        self.with(|device| device.mac())
    }

    fn can_tx(&self) -> bool {
        self.with(|device| device.can_tx())
    }

    fn can_rx(&self) -> bool {
        self.with(|device| device.can_rx())
    }

    fn rx_queue_len(&self) -> usize {
        self.with(|device| device.rx_queue_len())
    }

    fn tx_queue_len(&self) -> usize {
        self.with(|device| device.tx_queue_len())
    }

    fn recycle_rx(&mut self, rx_buf: NetBufHandle) -> DriverResult {
        self.with_mut(|device| device.recycle_rx(rx_buf))
    }

    fn recycle_tx(&mut self) -> DriverResult {
        self.with_mut(|device| device.recycle_tx())
    }

    fn send(&mut self, tx_buf: NetBufHandle) -> DriverResult {
        self.with_mut(|device| device.send(tx_buf))
    }

    fn recv(&mut self) -> DriverResult<NetBufHandle> {
        self.with_mut(|device| device.recv())
    }

    fn alloc_tx_buf(&mut self, size: usize) -> DriverResult<NetBufHandle> {
        self.with_mut(|device| device.alloc_tx_buf(size))
    }
}

#[cfg(feature = "vsock")]
impl VsockDevice for ClassDevice<VsockDeviceImpl> {
    fn guest_cid(&self) -> u64 {
        self.with(|device| device.guest_cid())
    }

    fn listen(&mut self, src_port: u32) {
        self.with_mut(|device| device.listen(src_port));
    }

    fn connect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        self.with_mut(|device| device.connect(cid))
    }

    fn send(&mut self, cid: VsockConnId, buf: &[u8]) -> DriverResult<usize> {
        self.with_mut(|device| device.send(cid, buf))
    }

    fn recv(&mut self, cid: VsockConnId, buf: &mut [u8]) -> DriverResult<usize> {
        self.with_mut(|device| device.recv(cid, buf))
    }

    fn recv_avail(&mut self, cid: VsockConnId) -> DriverResult<usize> {
        self.with_mut(|device| device.recv_avail(cid))
    }

    fn disconnect(&mut self, cid: VsockConnId) -> DriverResult<()> {
        self.with_mut(|device| device.disconnect(cid))
    }

    fn abort(&mut self, cid: VsockConnId) -> DriverResult<()> {
        self.with_mut(|device| device.abort(cid))
    }

    fn poll_event(&mut self) -> DriverResult<Option<VsockDriverEventType>> {
        self.with_mut(|device| device.poll_event())
    }
}

#[cfg(feature = "virtio-9p")]
impl ClassDevice<Virtio9pDeviceImpl> {
    pub fn mount_tag(&self) -> String {
        self.with(|device| device.mount_tag())
    }
}

#[cfg(feature = "virtio-9p")]
impl Virtio9pDevice for ClassDevice<Virtio9pDeviceImpl> {
    fn mount_tag(&self) -> String {
        self.mount_tag()
    }

    fn request(&mut self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.with_mut(|device| device.request(req, resp))
    }
}

class_registries! {
    "net",
    NetDeviceImpl,
    DeviceKind::Net,
    NET_DEVICES,
    net_class_registry,
    publish_net,
    net_devices,
    find_net_device,
    subscribe_net_available,
    notify_net_available,
    remove_net_device;
    "block",
    BlockDeviceImpl,
    DeviceKind::Block,
    BLOCK_DEVICES,
    block_class_registry,
    publish_block,
    block_devices,
    find_block_device,
    subscribe_block_available,
    notify_block_available,
    remove_block_device;
    "char",
    CharDeviceImpl,
    DeviceKind::Char,
    CHAR_DEVICES,
    char_class_registry,
    publish_char,
    char_devices,
    find_char_device,
    subscribe_char_available,
    notify_char_available,
    remove_char_device;
    "display",
    DisplayDeviceImpl,
    DeviceKind::Display,
    DISPLAY_DEVICES,
    display_class_registry,
    publish_display,
    display_devices,
    find_display_device,
    subscribe_display_available,
    notify_display_available,
    remove_display_device;
    "input",
    InputDeviceImpl,
    DeviceKind::Input,
    INPUT_DEVICES,
    input_class_registry,
    publish_input,
    input_devices,
    find_input_device,
    subscribe_input_available,
    notify_input_available,
    remove_input_device;
    "vsock",
    VsockDeviceImpl,
    DeviceKind::Vsock,
    VSOCK_DEVICES,
    vsock_class_registry,
    publish_vsock,
    vsock_devices,
    find_vsock_device,
    subscribe_vsock_available,
    notify_vsock_available,
    remove_vsock_device;
    "virtio-9p",
    Virtio9pDeviceImpl,
    DeviceKind::Fs9p,
    VIRTIO_9P_DEVICES,
    virtio_9p_class_registry,
    publish_virtio_9p,
    virtio_9p_devices,
    find_virtio_9p_device,
    subscribe_virtio_9p_available,
    notify_virtio_9p_available,
    remove_virtio_9p_device;
}

/// Shared class prelude for runtime publishers and consumers.
pub mod prelude {
    pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

    #[cfg(feature = "block")]
    pub use crate::{BlockDevice, BlockDeviceImpl};
    #[cfg(feature = "char")]
    pub use crate::{CharDevice, CharDeviceImpl};
    #[cfg(feature = "display")]
    pub use crate::{DisplayDevice, DisplayDeviceImpl, DisplayInfo};
    #[cfg(feature = "input")]
    pub use crate::{Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
    #[cfg(feature = "net")]
    pub use crate::{NetBufHandle, NetDevice, NetDeviceImpl};
    #[cfg(feature = "virtio-9p")]
    pub use crate::{Virtio9pDevice, Virtio9pDeviceImpl};
    #[cfg(feature = "vsock")]
    pub use crate::{VsockAddr, VsockConnId, VsockDevice, VsockDeviceImpl, VsockDriverEventType};
}
