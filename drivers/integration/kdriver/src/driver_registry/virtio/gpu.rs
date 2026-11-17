// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wire the GPU command waiter and the independent display-change worker.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{DriverError, DriverResult};
use driver_os_api::{IrqEvent, IrqResource};
use kclass::prelude::DisplayDevice;
use kdevice::ProbeContext;
use kspin::SpinNoPreempt;
use virtio::{GpuHost, Transport, VirtIoGpuDev};

use super::glue::VirtIoHalImpl;
use crate::resource::DeviceResourceExt;

struct Host(ktask::WaitQueue);

impl GpuHost for Host {
    type Mutex = ksync::RawMutex;

    fn can_sleep(&self) -> bool {
        ktask::is_sleepable_context()
    }

    fn wait_until(&self, ready: impl FnMut() -> bool) {
        self.0.wait_until(ready);
    }

    fn notify(&self) {
        self.0.notify_one(false);
    }
}

pub(super) fn activate<T: Transport + Send + 'static>(
    ctx: &ProbeContext<'_>,
    transport: T,
    irq: Option<IrqResource>,
    pci: Option<(&mut pci::PciConfigAccess, pci::DeviceFunction)>,
) -> DriverResult<()> {
    let irq = irq.ok_or(DriverError::InvalidInput)?;
    let gpu =
        VirtIoGpuDev::<VirtIoHalImpl, _, _>::prepare(transport, Host(ktask::WaitQueue::new()))?;
    let publication = Arc::new(SpinNoPreempt::new(None));
    let work_gpu = gpu.clone();
    let work_publication = publication.clone();
    let work = kworkng::WorkHandle::new(move || {
        let Some(publication) = *work_publication.lock() else {
            return;
        };
        if kclass::find_display_publication(publication).is_some()
            && work_gpu.refresh_info().is_some()
            && let Some(node) = kdevice::device_nodes_snapshot()
                .into_iter()
                .find(|n| n.publication_id() == publication)
        {
            kdevice_uevent::publish_drm_hotplug(&node);
        }
    })
    .map_err(|_| DriverError::NoMemory)?;
    let refresh = work.clone();
    let handler = gpu.clone();
    let virq = irq.virq().raw();
    ctx.devm_request_irq(
        irq,
        Arc::new(move |line| {
            if line != virq {
                return IrqEvent::NOT_HANDLED;
            }
            let status = handler.ack_irq();
            if status.is_empty() {
                return IrqEvent::NOT_HANDLED;
            }
            if status.config {
                let _ = refresh.queue_default(None);
            }
            IrqEvent::HANDLED
        }),
    )?;
    if let Some((config, bdf)) = pci {
        super::arm_pci_intx(ctx, config, bdf)?;
    }
    let closing = gpu.clone();
    ctx.add_cleanup(move || closing.close())?;
    gpu.start()?;
    log::info!("virtio-gpu: display discovery complete on IRQ {virq}");
    let display = kclass::publish_display(ctx, Box::new(gpu))?;
    *publication.lock() = Some(display.publication_id());
    // A config interrupt may have arrived before the class was visible.
    let _ = work.queue_default(None);
    Ok(())
}
