// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned virtio block activation, publication and drained removal.

use alloc::sync::Arc;
#[cfg(unittest)]
use core::sync::atomic::{AtomicBool, Ordering};

use block::{Gendisk, GendiskRef, completion::BlockCompletionOperations};
use driver_base::{Device, DriverError, DriverResult};
use driver_os_api::{DevIrq, IrqHandler, IrqOp, IrqResource};
use kdevice::ProbeContext;
use virtio::{Transport, VirtIoBlkDev, VirtIoHal};

use super::glue::VirtIoHalImpl;
use crate::{block_completion, block_completion_dispatch::BlockIoReclaimer, block_irq};

/// Close operations needed after the transport type is erased.
pub(super) trait BlockActivationOps: Send + Sync {
    fn begin_close(&self, notification: Arc<dyn block::completion::BlockSignals>);
    fn disable_interrupts(&self);
    fn finish_close(&self);
}

impl<H: VirtIoHal + 'static, T: Transport + 'static> BlockActivationOps for VirtIoBlkDev<H, T> {
    fn begin_close(&self, notification: Arc<dyn block::completion::BlockSignals>) {
        VirtIoBlkDev::begin_close(self, notification)
    }

    fn disable_interrupts(&self) {
        VirtIoBlkDev::disable_interrupts(self)
    }

    fn finish_close(&self) {
        VirtIoBlkDev::finish_close(self)
    }
}

pub(super) struct BlockActivation {
    device: Arc<dyn BlockActivationOps>,
    irq: Option<DevIrq>,
    reclaimer: Arc<BlockIoReclaimer>,
    disk: Option<GendiskRef>,
    closed: bool,
}

impl BlockActivation {
    pub(super) fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
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

impl Drop for BlockActivation {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(unittest)]
static FAIL_NEXT_DISK_ALLOC: AtomicBool = AtomicBool::new(false);

#[cfg(unittest)]
pub(super) fn fail_next_disk_alloc() {
    FAIL_NEXT_DISK_ALLOC.store(true, Ordering::Release);
}

#[cfg(unittest)]
fn take_disk_alloc_failure() -> bool {
    FAIL_NEXT_DISK_ALLOC.swap(false, Ordering::AcqRel)
}

pub(super) fn activate<T: Transport + 'static>(
    ctx: &ProbeContext<'_>,
    transport: T,
    irq: Option<IrqResource>,
) -> DriverResult<BlockActivation> {
    let irq = irq.ok_or(DriverError::InvalidInput)?;
    let device = Arc::new(VirtIoBlkDev::<VirtIoHalImpl, T>::try_new_irq(transport)?);
    activate_device(ctx, device, irq, crate::resource::resource_provider())
}

fn activate_device<H: VirtIoHal + 'static, T: Transport + 'static>(
    ctx: &ProbeContext<'_>,
    device: Arc<VirtIoBlkDev<H, T>>,
    irq: IrqResource,
    provider: &'static dyn IrqOp,
) -> DriverResult<BlockActivation> {
    let completion: Arc<dyn BlockCompletionOperations> = device.clone();
    let reclaimer = BlockIoReclaimer::new(Arc::downgrade(&completion));
    let close_device: Arc<dyn BlockActivationOps> = device.clone();
    let mut activation = BlockActivation {
        device: close_device,
        irq: None,
        reclaimer,
        disk: None,
        closed: false,
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
    #[cfg(unittest)]
    if take_disk_alloc_failure() {
        return Err(DriverError::NoMemory);
    }
    let disk = Gendisk::alloc_mq(
        device.name().into(),
        virtio::VIRTIO_BLK_MAJOR,
        device.index() << virtio::VIRTIO_BLK_PART_BITS,
        1 << virtio::VIRTIO_BLK_PART_BITS,
        device.clone(),
    )?;
    activation.disk = Some(disk.clone());
    device.enable_interrupts();
    kclass::publish_block(ctx, disk)?;
    Ok(activation)
}

unittest::path_mod_test!(
    virtio_block_activation_unittest,
    "../../../ut-tests/virtio_block_activation_unittest.rs"
);
