// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Typed runtime device classes built on top of the device core.
//!
//! This crate owns the per-class runtime registries, publication/lookup,
//! availability notifications and the activation bridge from `kdevice` events.
//! It is framework core rather than a set of pure class contracts.
//!
//! The generic class-device handle and registry mechanics live in the internal
//! `generic` module. This crate root wires those mechanics to concrete
//! block/net/char/hwrng/display/input/vsock/9P operation traits.
//! Drivers publish runtime capabilities here after probe succeeds so subsystems
//! can enumerate existing devices and subscribe to later availability without
//! depending on probe order.
//! Input and display classes also translate their runtime metadata into
//! [`kdevice::DeviceNode`] registrations when the parent device becomes active.

//! ```compile_fail
//! fn cannot_publish_from_remove(ctx: &kdevice::RemoveContext<'_>) {
//!     let _ = kclass::publish_char(ctx, unreachable!());
//! }
//! ```

#![no_std]

extern crate alloc;

#[cfg(feature = "input")]
use alloc::collections::BTreeMap;
use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};

use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{DeviceEvent, DeviceEventKind, DeviceId, ProbeContext};
use kspin::SpinNoPreempt;
use lazyinit::LazyInit;

mod generic;

#[cfg(unittest)]
#[path = "retirement_hooks.rs"]
pub mod test_hooks;

/// Re-export generic class-device primitives that are part of the public API.
pub use self::generic::{ClassAvailabilityCallback, ClassDevice};
use self::generic::{ClassDeviceMetadata, ClassRegistry};

static ACTIVATION_BRIDGE: LazyInit<()> = LazyInit::new();

fn subscribe_bridge() {
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
    // A candidate binding that failed before activation may already have
    // published a class runtime. Its terminal entry is revoked through the same
    // exact-binding path as a published removal.
    kdevice::subscribe_device_event_kind(
        DeviceEventKind::BindingRetired,
        Arc::new(|event| {
            let DeviceEvent::BindingRetired { id } = event else {
                return;
            };
            remove_class_device(id);
        }),
    );
}

fn ensure_event_bridge() {
    ACTIVATION_BRIDGE.call_once(subscribe_bridge);
}

/// Reinstall the production subscriptions after a test reset cleared them.
///
/// The one-shot `ACTIVATION_BRIDGE` initializer cannot be reset, so tests that
/// clear subscriber buckets call this instead of hand-removing class entries.
#[cfg(unittest)]
pub fn reinstall_event_bridge_for_tests() {
    subscribe_bridge();
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
        $remove_fn:ident,
        $evict_fn:ident,
        $retire_fn:ident,
        $remove_any_fn:ident
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
        ///
        /// Publish from the exact probe context. The class handle captures that
        /// context's binding, so a later replacement binding never inherits
        /// this handle's identity.
        #[cfg(feature = $feature)]
        pub fn $publish_fn(ctx: &ProbeContext<'_>, runtime: $ty) -> DriverResult<ClassDevice<$ty>> {
            let parent = ctx.device().clone();
            let binding = ctx.binding().clone();
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
            let device = ClassDevice::try_new_with_binding(
                parent, binding, runtime, name, $kind, irq, metadata,
            )?;
            let id = device.id();

            // Reject an in-flight retirement or a live same-ID entry before
            // touching class resources. Terminal entries are then evicted in
            // the fixed order node -> unpublish -> registry removal.
            let _ = $registry_fn().lock().terminal_entries_for_retirement(id)?;
            $evict_fn(id)?;

            device.with(ClassRuntimeMetadata::publish_class)?;
            let is_active = device.is_available();
            if let Err(error) = $registry_fn().lock().publish(device.clone()) {
                device.with(ClassRuntimeMetadata::unpublish_class);
                return Err(error);
            }
            log::debug!(
                "device class '{}': published {:?} (driver={})",
                $kind.as_str(),
                id,
                device.driver_name(),
            );
            if is_active {
                notify_class_available($kind, id);
            }
            Ok(device)
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

        /// Claim the terminal class entry for `binding` and unpublish it
        /// outside the registry lock.
        ///
        /// This is the single retirement path used by synchronous eviction and
        /// by deferred node completion. Only the executor that receives
        /// `Claimed` unpublishes; the reservation blocks same-ID publication
        /// until `finish_retirement` clears it.
        #[cfg(feature = $feature)]
        fn $retire_fn(id: DeviceId, binding: kdevice::BindingRef) -> DriverResult<()> {
            let claim = $registry_fn()
                .lock()
                .claim_retirement(id, core::slice::from_ref(&binding))?;
            if matches!(&claim, crate::generic::ClaimOutcome::Claimed { .. }) {
                #[cfg(unittest)]
                crate::test_hooks::pause(crate::test_hooks::ClassPause::RetirementClaimed { id });
                for device in claim.removed() {
                    device.with(ClassRuntimeMetadata::unpublish_class);
                }
                #[cfg(unittest)]
                crate::test_hooks::pause(crate::test_hooks::ClassPause::RetirementUnpublished {
                    id,
                });
                $registry_fn().lock().finish_retirement(&claim);
                Ok(())
            } else if matches!(&claim, crate::generic::ClaimOutcome::Retiring) {
                Err(DriverError::ResourceBusy)
            } else {
                Ok(())
            }
        }

        /// Retire every terminal entry for `id`, preserving the visible order
        /// node revocation -> unpublish -> reservation release.
        #[cfg(feature = $feature)]
        fn $evict_fn(id: DeviceId) -> DriverResult<()> {
            let terminals = $registry_fn().lock().terminal_entries_for_retirement(id)?;
            if terminals.is_empty() {
                return Ok(());
            }
            for device in terminals {
                let binding = device.binding_ref().clone();
                let on_deferred = {
                    let binding = binding.clone();
                    Box::new(move || {
                        if let Err(error) = $retire_fn(id, binding) {
                            log::error!(
                                "device class '{}': deferred retirement of {:?} failed: {}",
                                $kind.as_str(),
                                id,
                                error,
                            );
                        }
                    })
                };
                match kdevice::unregister_device_node_for_binding(id, &binding, on_deferred) {
                    kdevice::NodeRemoval::Removed | kdevice::NodeRemoval::NotFound => {}
                    kdevice::NodeRemoval::Deferred | kdevice::NodeRemoval::DeferredExisting => {
                        return Err(DriverError::ResourceBusy);
                    }
                    kdevice::NodeRemoval::NotOwner => {
                        log::error!(
                            "device class '{}': device node for {:?} belongs to another binding",
                            $kind.as_str(),
                            id,
                        );
                        return Err(DriverError::BadState);
                    }
                }
                // Retire each entry as soon as its node is gone, so a later
                // deferred entry cannot strand the earlier terminal entries.
                $retire_fn(id, binding)?;
            }
            Ok(())
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

            if let Some(node_class) = device.with(ClassRuntimeMetadata::device_node_class)
                && let Err(error) = kdevice::register_device_node_for_binding(
                    id,
                    device.binding_ref(),
                    node_class,
                    String::from(device.name()),
                    String::from(device.driver_name()),
                    device.location(),
                    match device.parent().identity() {
                        kdevice::DeviceIdentity::Pci(pci) => Some(pci),
                        kdevice::DeviceIdentity::Platform(_) => None,
                    },
                )
            {
                log::warn!(
                    "device class '{}': failed to assign a device node for {:?}: {}",
                    $kind.as_str(),
                    id,
                    error,
                );
            }

            for callback in callbacks {
                callback(device.clone());
            }
        }

        #[cfg(feature = $feature)]
        fn $remove_fn(id: DeviceId) {
            if let Err(error) = $evict_fn(id) {
                // `ResourceBusy` with a registered deferred completion is the
                // normal node-in-flight outcome; the node finishing path owns
                // the eventual unpublish. Only unexpected eviction failures
                // deserve operator-visible warning.
                if matches!(&error, DriverError::ResourceBusy) {
                    log::debug!(
                        "device class '{}': retirement of {:?} deferred: {}",
                        $kind.as_str(),
                        id,
                        error,
                    );
                } else {
                    log::warn!(
                        "device class '{}': retired device {:?} not fully cleaned: {}",
                        $kind.as_str(),
                        id,
                        error,
                    );
                }
            }
        }

        /// Test cleanup: run the production retirement path, then drop any
        /// leftover entries for `id`.
        ///
        /// The caller must have joined every task that could still hold a
        /// retirement claim. Deferred node completions keep their entries (and
        /// their registered callback); this helper does not force-clear them.
        #[cfg(all(feature = $feature, unittest))]
        fn $remove_any_fn(id: DeviceId) {
            $remove_fn(id);
            let entries = $registry_fn().lock().entries_for_tests(id);
            let mut deferred: Vec<kdevice::BindingRef> = Vec::new();
            for device in &entries {
                let binding = device.binding_ref().clone();
                let on_deferred = {
                    let binding = binding.clone();
                    Box::new(move || {
                        if let Err(error) = $retire_fn(id, binding) {
                            log::error!(
                                "device class '{}': deferred test retirement of {:?} failed: {}",
                                $kind.as_str(),
                                id,
                                error,
                            );
                        }
                    })
                };
                match kdevice::unregister_device_node_for_binding(id, &binding, on_deferred) {
                    kdevice::NodeRemoval::Removed | kdevice::NodeRemoval::NotFound => {}
                    kdevice::NodeRemoval::Deferred | kdevice::NodeRemoval::DeferredExisting => {
                        deferred.push(binding);
                    }
                    kdevice::NodeRemoval::NotOwner => log::error!(
                        "device class '{}': test cleanup found a foreign node owner for {:?}",
                        $kind.as_str(),
                        id,
                    ),
                }
            }
            for device in $registry_fn().lock().remove_any_for_tests(id, &deferred) {
                device.with(ClassRuntimeMetadata::unpublish_class);
            }
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
            $remove_fn:ident,
            $evict_fn:ident,
            $retire_fn:ident,
            $remove_any_fn:ident
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

        /// Remove class entries for `id` without going through the device-event
        /// bridge.
        ///
        /// Tests that reset the device registry clear the bridge's one-shot
        /// subscriptions but cannot reset the one-shot initializer. They may
        /// call this after `remove_device_managed`, once every task that could
        /// still hold a retirement claim has been joined.
        #[cfg(unittest)]
        pub fn remove_class_device_for_tests(id: DeviceId) {
            $(
                #[cfg(feature = $feature)]
                $remove_any_fn(id);
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
                $remove_fn,
                $evict_fn,
                $retire_fn,
                $remove_any_fn
            );
        )+
    };
}

/// Concrete runtime type for network devices.
///
/// Drivers construct this via `Box::new(their_net_device)` and publish it
/// through [`publish_net`].
#[cfg(feature = "net")]
pub type NetDeviceImpl = Box<dyn NetDevice + Send + Sync>;
/// Concrete runtime type for registered generic disks.
#[cfg(feature = "block")]
pub type BlockDeviceImpl = block::GendiskRef;
/// Concrete runtime type for character devices.
#[cfg(feature = "char")]
pub type CharDeviceImpl = Box<dyn CharDevice + Send + Sync>;
/// Shared hardware entropy source, also retained by its host activation owner.
///
/// Publish only after the driver can receive completion IRQs and accept reads.
/// Registry removal does not stop the source or prove that DMA has stopped.
#[cfg(feature = "hwrng")]
pub type HwrngDeviceImpl = Arc<dyn HwrngDevice + Send + Sync>;
/// Concrete runtime type for display devices.
#[cfg(feature = "display")]
pub type DisplayDeviceImpl = Box<dyn DisplayDevice + Send + Sync>;
/// Concrete runtime type for input devices.
#[cfg(feature = "input")]
pub type InputDeviceImpl = Box<dyn InputDevice + Send + Sync>;
/// Concrete runtime type for VSOCK devices.
#[cfg(feature = "vsock")]
pub type VsockDeviceImpl = Box<dyn VsockDevice + Send + Sync>;
/// Concrete runtime type for VirtIO 9P devices.
#[cfg(feature = "virtio-9p")]
pub type Virtio9pDeviceImpl = Box<dyn Virtio9pDevice + Send + Sync>;

/// Re-export the generic disk type used by block-class publishers.
#[cfg(feature = "block")]
pub use block::Gendisk;
/// Re-export character device trait.
#[cfg(feature = "char")]
pub use char_driver::CharDevice;
/// Re-export display device trait and framebuffer metadata.
#[cfg(feature = "display")]
pub use display::{DisplayDevice, DisplayInfo};
/// Re-export the hardware entropy source contract.
#[cfg(feature = "hwrng")]
pub use hwrng::HwrngDevice;
/// Re-export input device trait, event types, identity types, and axis info.
#[cfg(feature = "input")]
pub use input::{
    ABS_BITMAP_LEN, ABS_CNT, ABS_MT_FIRST, ABS_MT_LAST, ABS_MT_SLOT, AbsInfo, Event, EventType,
    InputDevice, InputDeviceId, InputEventNotifier, PROP_BITMAP_LEN,
};
/// Re-export network device trait, buffer handle, and RX scheduler capability.
#[cfg(feature = "net")]
pub use net::{NetBufHandle, NetDevice, NetRxScheduler};
/// Re-export VirtIO 9P device trait.
#[cfg(feature = "virtio-9p")]
pub use virtio::Virtio9pDevice;
/// Re-export VSOCK device trait and address/connection types.
#[cfg(feature = "vsock")]
pub use vsock::{
    VsockAddr, VsockConnId, VsockConnectionInfo, VsockDevice, VsockTransportEvent,
    VsockTransportEventKind,
};

trait ClassRuntimeMetadata {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::empty()
    }

    fn publish_class(&self) -> DriverResult<()> {
        Ok(())
    }

    fn unpublish_class(&self) {}

    fn device_node_class(&self) -> Option<kdevice::DeviceNodeClass> {
        None
    }
}

#[cfg(feature = "block")]
impl ClassRuntimeMetadata for BlockDeviceImpl {
    fn publish_class(&self) -> DriverResult<()> {
        block::add_disk(self.clone()).map(drop)
    }

    fn unpublish_class(&self) {
        block::del_gendisk(self.device_number());
    }
}

#[cfg(feature = "char")]
impl ClassRuntimeMetadata for CharDeviceImpl {}

#[cfg(feature = "hwrng")]
impl ClassRuntimeMetadata for HwrngDeviceImpl {}

#[cfg(feature = "display")]
impl ClassRuntimeMetadata for DisplayDeviceImpl {
    fn device_node_class(&self) -> Option<kdevice::DeviceNodeClass> {
        // The connector snapshot is the same information `GETCONNECTOR`
        // reports: one virtual connector, named after its type.
        let info = self.info();
        let edid = self.edid().unwrap_or_default();
        let status = if self.is_connected() {
            "connected"
        } else {
            "disconnected"
        };
        let modes = if self.is_connected() {
            format!("{}x{}\n", info.width, info.height)
        } else {
            String::new()
        };
        Some(kdevice::DeviceNodeClass::Drm(
            kdevice::DrmDeviceNodeMetadata::new(
                display::connector_name(
                    display::VIRTUAL_CONNECTOR_NAME,
                    display::VIRTUAL_CONNECTOR_TYPE_ID,
                ),
                String::from(status),
                String::from("On"),
                modes,
                edid,
            ),
        ))
    }
}

#[cfg(feature = "input")]
impl ClassRuntimeMetadata for InputDeviceImpl {
    fn class_metadata(&self) -> ClassDeviceMetadata {
        ClassDeviceMetadata::input(self.physical_location().into(), self.unique_id().into())
    }

    fn device_node_class(&self) -> Option<kdevice::DeviceNodeClass> {
        let mut event_types = alloc::vec![0; (EventType::COUNT as usize).div_ceil(8)];
        let mut event_codes = BTreeMap::new();
        for raw in 0..EventType::COUNT {
            let Some(event_type) = EventType::from_repr(raw) else {
                continue;
            };
            let mut codes = alloc::vec![0; event_type.bits_count().div_ceil(8)];
            if self
                .get_event_bits(event_type, &mut codes)
                .is_ok_and(|is_supported| is_supported)
            {
                event_types[raw as usize / 8] |= 1 << (raw as usize % 8);
                event_codes.insert(raw, codes);
            }
        }
        let id = self.device_id();
        Some(kdevice::DeviceNodeClass::Input(
            kdevice::InputDeviceNodeMetadata::new(
                String::from(self.name()),
                String::from(self.physical_location()),
                String::from(self.unique_id()),
                kdevice::InputDeviceNodeId::new(id.bus_type, id.vendor, id.product, id.version),
                event_types,
                event_codes,
            ),
        ))
    }
}

#[cfg(feature = "net")]
impl ClassRuntimeMetadata for NetDeviceImpl {}

#[cfg(feature = "vsock")]
impl ClassRuntimeMetadata for VsockDeviceImpl {}

#[cfg(feature = "virtio-9p")]
impl ClassRuntimeMetadata for Virtio9pDeviceImpl {}

#[cfg(feature = "char")]
impl CharDevice for ClassDevice<CharDeviceImpl> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.read(buf))
    }

    fn write(&self, buf: &[u8]) -> DriverResult<usize> {
        self.with(|device| device.write(buf))
    }

    fn flush(&self) -> DriverResult {
        self.with(|device| device.flush())
    }
}

#[cfg(feature = "hwrng")]
impl HwrngDevice for ClassDevice<HwrngDeviceImpl> {
    fn read(&self, buf: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.read(buf))
    }

    fn is_failed(&self) -> bool {
        self.with(|device| device.is_failed())
    }
}

#[cfg(feature = "display")]
impl DisplayDevice for ClassDevice<DisplayDeviceImpl> {
    fn info(&self) -> DisplayInfo {
        self.with(|device| device.info())
    }

    fn driver_info(&self) -> display::DriverInfo {
        self.with(|device| device.driver_info())
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        self.with(|device| device.refresh_info())
    }

    fn ack_irq(&self) -> display::DisplayIrqStatus {
        self.with(|device| device.ack_irq())
    }

    fn need_flush(&self) -> bool {
        self.with(|device| device.need_flush())
    }

    fn flush(&self) -> DriverResult {
        self.with(|device| device.flush())
    }

    fn create_scanout_resource(
        &self,
        resource: display::ScanoutResource,
        window: &display::ScanoutWindow<'_>,
    ) -> DriverResult {
        self.with(|device| device.create_scanout_resource(resource, window))
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        self.with(|device| device.destroy_scanout_resource(resource_id))
    }

    fn present_scanout_resource(
        &self,
        resource_id: u32,
        rect: display::ScanoutRect,
        host_rendered: bool,
    ) -> DriverResult {
        self.with(|device| device.present_scanout_resource(resource_id, rect, host_rendered))
    }

    fn flush_scanout_resource(
        &self,
        resource_id: u32,
        rect: display::ScanoutRect,
        host_rendered: bool,
    ) -> DriverResult {
        self.with(|device| device.flush_scanout_resource(resource_id, rect, host_rendered))
    }

    fn vblank_source(&self) -> Option<&dyn display::VblankSource> {
        // The class handle owns the driver object that implements the source,
        // and this select ties the borrow to the handle's lifetime.
        self.select(
            |device| device.vblank_source(),
            |source| source.map(|source| source as &dyn display::VblankSource),
        )
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

    fn get_event_bits(&self, ty: EventType, out: &mut [u8]) -> DriverResult<bool> {
        self.with(|device| device.get_event_bits(ty, out))
    }

    fn abs_info(&self, code: u16) -> DriverResult<Option<AbsInfo>> {
        self.with(|device| device.abs_info(code))
    }

    fn get_prop_bits(&self, out: &mut [u8]) -> DriverResult<bool> {
        self.with(|device| device.get_prop_bits(out))
    }

    fn read_event(&self) -> DriverResult<Event> {
        self.with(|device| device.read_event())
    }

    fn poll_event_ready(&self) -> Option<bool> {
        self.with(|device| device.poll_event_ready())
    }

    fn set_event_notifier(&self, notifier: Option<Arc<dyn InputEventNotifier>>) -> DriverResult {
        self.with(|device| device.set_event_notifier(notifier))
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

    fn recycle_rx(&self, rx_buf: NetBufHandle) -> DriverResult {
        self.with(|device| device.recycle_rx(rx_buf))
    }

    fn recycle_tx(&self) -> DriverResult {
        self.with(|device| device.recycle_tx())
    }

    fn send(&self, tx_buf: NetBufHandle) -> DriverResult {
        self.with(|device| device.send(tx_buf))
    }

    fn recv(&self) -> DriverResult<NetBufHandle> {
        self.with(|device| device.recv())
    }

    fn alloc_tx_buf(&self, size: usize) -> DriverResult<NetBufHandle> {
        self.with(|device| device.alloc_tx_buf(size))
    }

    fn set_rx_scheduler(&self, scheduler: Option<Arc<dyn NetRxScheduler>>) -> DriverResult {
        self.with(|device| device.set_rx_scheduler(scheduler))
    }
}

#[cfg(feature = "vsock")]
impl VsockDevice for ClassDevice<VsockDeviceImpl> {
    fn guest_cid(&self) -> u64 {
        self.with(|device| device.guest_cid())
    }

    fn listen(&self, port: u32) {
        self.with(|device| device.listen(port));
    }

    fn unlisten(&self, port: u32) {
        self.with(|device| device.unlisten(port));
    }

    fn connect(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.connect(info))
    }

    fn accept(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.accept(info))
    }

    fn force_close(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.force_close(info))
    }

    fn send(&self, info: &VsockConnectionInfo, buf: &[u8]) -> DriverResult<usize> {
        self.with(|device| device.send(info, buf))
    }

    fn shutdown(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.shutdown(info))
    }

    fn credit_update(&self, info: &VsockConnectionInfo) -> DriverResult<()> {
        self.with(|device| device.credit_update(info))
    }

    fn poll_event(
        &self,
        handler: &mut dyn FnMut(VsockTransportEvent, &[u8]) -> DriverResult<()>,
    ) -> DriverResult<bool> {
        self.with(|device| device.poll_event(handler))
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

    fn request(&self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.with(|device| device.request(req, resp))
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
    remove_net_device,
    evict_net_terminal,
    retire_net_binding,
    remove_net_class_any;
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
    remove_block_device,
    evict_block_terminal,
    retire_block_binding,
    remove_block_class_any;
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
    remove_char_device,
    evict_char_terminal,
    retire_char_binding,
    remove_char_class_any;
    "hwrng",
    HwrngDeviceImpl,
    DeviceKind::Hwrng,
    HWRNG_DEVICES,
    hwrng_class_registry,
    publish_hwrng,
    hwrng_devices,
    find_hwrng_device,
    subscribe_hwrng_available,
    notify_hwrng_available,
    remove_hwrng_device,
    evict_hwrng_terminal,
    retire_hwrng_binding,
    remove_hwrng_class_any;
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
    remove_display_device,
    evict_display_terminal,
    retire_display_binding,
    remove_display_class_any;
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
    remove_input_device,
    evict_input_terminal,
    retire_input_binding,
    remove_input_class_any;
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
    remove_vsock_device,
    evict_vsock_terminal,
    retire_vsock_binding,
    remove_vsock_class_any;
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
    remove_virtio_9p_device,
    evict_virtio_9p_terminal,
    retire_virtio_9p_binding,
    remove_virtio_9p_class_any;
}

/// Shared class prelude for runtime publishers and consumers.
pub mod prelude {
    #[cfg(feature = "block")]
    pub use block::BlockDeviceOperations;
    pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

    #[cfg(feature = "block")]
    pub use crate::BlockDeviceImpl;
    #[cfg(feature = "char")]
    pub use crate::{CharDevice, CharDeviceImpl};
    #[cfg(feature = "display")]
    pub use crate::{DisplayDevice, DisplayDeviceImpl, DisplayInfo};
    #[cfg(feature = "input")]
    pub use crate::{Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
    #[cfg(feature = "hwrng")]
    pub use crate::{HwrngDevice, HwrngDeviceImpl};
    #[cfg(feature = "net")]
    pub use crate::{NetBufHandle, NetDevice, NetDeviceImpl, NetRxScheduler};
    #[cfg(feature = "virtio-9p")]
    pub use crate::{Virtio9pDevice, Virtio9pDeviceImpl};
    #[cfg(feature = "vsock")]
    pub use crate::{
        VsockAddr, VsockConnId, VsockConnectionInfo, VsockDevice, VsockDeviceImpl,
        VsockTransportEvent, VsockTransportEventKind,
    };
}

/// Test-only scoped publication helper. Production drivers use
/// [`publish_char`] with a `ProbeContext`.
#[cfg(all(unittest, feature = "char"))]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: test-only helper accepts the production boxed \
              CharDeviceImpl required by kclass publication"
)]
pub fn publish_char_for_test(
    parent: Arc<kdevice::DeviceObject>,
    runtime: CharDeviceImpl,
) -> DriverResult<ClassDevice<CharDeviceImpl>> {
    ProbeContext::with_test_context(parent, |ctx| publish_char(ctx, runtime))
        .unwrap_or(Err(DriverError::BadState))
}
