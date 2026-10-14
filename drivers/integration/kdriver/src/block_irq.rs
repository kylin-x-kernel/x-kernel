// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Connects one device's IRQ acknowledgement to its deferred I/O reclaiming.
//!
//! Sharing, action capacity and in-flight IRQ synchronization belong to kirq,
//! reached through the X-Kernel resource provider. No block-specific IRQ registry.

use alloc::sync::{Arc, Weak};

use device_res::{Irq, IrqEvent, IrqHandler, IrqOp, IrqResource, ResResult};

use crate::block_completion_dispatch::BlockIoReclaimer;

/// Registers one device through the host provider's shared IRQ support.
///
/// Task-context setup before disk publication. The device handler acknowledges
/// only its own interrupt without sleeping; claimed events mark the reclaimer.
/// Provider errors propagate without retry, grouping or polling fallback.
///
/// Activation keeps the device alive, drains admitted I/O and suppresses device
/// IRQ generation before dropping the returned Irq in sleepable task context.
/// The X-Kernel provider synchronizes in-flight IRQ callbacks during that Drop.
/// Only afterwards may activation stop the reclaimer and destroy the transport.
#[cfg_attr(all(not(unittest), not(feature = "virtio-blk")), expect(dead_code))]
pub(crate) fn request_block_irq(
    provider: &'static dyn IrqOp,
    resource: IrqResource,
    device: Weak<dyn IrqHandler>,
    reclaimer: Arc<BlockIoReclaimer>,
) -> ResResult<Irq> {
    Irq::request_with(
        provider,
        resource,
        Arc::new(move |irq| {
            let Some(device) = device.upgrade() else {
                return IrqEvent::NOT_HANDLED;
            };
            let event = device.handle(irq);
            if event.handled() {
                reclaimer.mark_pending();
            }
            event
        }),
    )
}

// Adapter tests: dispatch here is synchronous, so mock release has no escaped
// callbacks to wait for. Concurrent release is covered by kirq's own tests.
unittest::path_mod_test!(block_irq_unittest, "../ut-tests/block_irq_unittest.rs");
