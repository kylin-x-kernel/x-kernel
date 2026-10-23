// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned virtio block activation, publication and drained removal.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc};

use block::{Gendisk, GendiskRef, completion::BlockCompletionOperations};
use device_res::{Irq, IrqHandler, IrqOp, IrqResource, IrqTrigger};
use driver_base::{Device, DriverError, DriverResult};
use kdevice::{DeviceId, DeviceObject};
use ksync::{Mutex, static_lock};
use virtio::{Transport, VirtIoBlkDev, VirtIoHal};

use super::glue::VirtIoHalImpl;
use crate::{block_completion, block_completion_dispatch::BlockIoReclaimer, block_irq};

static_lock! {
    // Heterogeneous transports are erased only at task-context cleanup. IRQ and
    // I/O paths never look up this map or capture its owning DeviceObject.
    static ACTIVATIONS: Mutex<BTreeMap<DeviceId, Box<dyn FnOnce() + Send>>> =
        Mutex::new(BTreeMap::new());
}

struct BlockActivation<H: VirtIoHal, T: Transport> {
    device: Arc<VirtIoBlkDev<H, T>>,
    irq: Option<Irq>,
    reclaimer: Arc<BlockIoReclaimer>,
    disk: Option<GendiskRef>,
}

impl<H: VirtIoHal, T: Transport> Drop for BlockActivation<H, T> {
    fn drop(&mut self) {
        // Prepare on the removing task, not the original probe task. No
        // recoverable failure may let device-core continue tearing down live DMA.
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Device removal runs in sleepable task \
                      context; each newly created completion has a fresh ID and four inline \
                      waiter slots"
        )]
        let mut drain = block_completion::prepare_block_wait().expect("block close task context");
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: Device removal runs in sleepable task \
                      context; each newly created completion has a fresh ID and four inline \
                      waiter slots"
        )]
        let mut stop = block_completion::prepare_block_wait().expect("block close task context");
        self.device.begin_close(drain.signals());
        if let Some(disk) = &self.disk {
            block::del_gendisk(disk.device_number());
        }
        drain.wait_completion();
        self.device.disable_interrupts();
        drop(self.irq.take());
        self.reclaimer.stop_and_wait(stop.as_mut());
        self.device.finish_close();
    }
}

pub(super) fn activate<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: Option<usize>,
) -> DriverResult<()> {
    let irq = irq.ok_or(DriverError::InvalidInput)?;
    let device = Arc::new(VirtIoBlkDev::<VirtIoHalImpl, T>::try_new_irq(
        transport,
        block_completion::prepare_block_wait,
    )?);
    // PCI/MMIO discovery already mapped this virtual IRQ. A plain resource
    // reuses that mapping; Unknown does not replace its trigger/polarity.
    activate_device(
        parent,
        device,
        IrqResource::new(irq, IrqTrigger::Unknown(0)),
        crate::resource::resource_provider(),
    )
}

fn activate_device<H: VirtIoHal + 'static, T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    device: Arc<VirtIoBlkDev<H, T>>,
    irq: IrqResource,
    provider: &'static dyn IrqOp,
) -> DriverResult<()> {
    let completion: Arc<dyn BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&completion));
    let mut activation = BlockActivation {
        device: device.clone(),
        irq: None,
        reclaimer,
        disk: None,
    };
    let handler: Arc<dyn IrqHandler> = device.clone();
    activation.irq = Some(
        block_irq::request_block_irq(
            provider,
            irq,
            Arc::downgrade(&handler),
            activation.reclaimer.clone(),
        )
        .map_err(crate::resource::map_res_err)?,
    );
    let disk = Gendisk::alloc(
        device.name().into(),
        virtio::VIRTIO_BLK_MAJOR,
        device.index() << virtio::VIRTIO_BLK_PART_BITS,
        1 << virtio::VIRTIO_BLK_PART_BITS,
        Box::new(device.clone()),
    )?;
    activation.disk = Some(disk.clone());
    device.enable_interrupts();
    let id = parent.id();
    {
        let mut registry = ACTIVATIONS.lock();
        if registry.contains_key(&id) {
            return Err(DriverError::AlreadyExists);
        }
        registry.insert(id, Box::new(move || drop(activation)));
    }
    parent.add_cleanup(move || close_device(id));
    if let Err(error) = kclass::publish_block(parent, disk) {
        close_device(id);
        return Err(error);
    }
    Ok(())
}

/// Device-core serializes remove via begin_removing; its later devres fallback
/// calls this again after remove returns. Taking the unique owner is idempotent.
pub(super) fn close_device(id: DeviceId) {
    let close = { ACTIVATIONS.lock().remove(&id) };
    if let Some(close) = close {
        close();
    }
}

unittest::path_mod_test!(
    virtio_block_activation_unittest,
    "../../../ut-tests/virtio_block_activation_unittest.rs"
);
