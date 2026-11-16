// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input activation and host interrupt-resource ownership.

use alloc::boxed::Box;

use driver_base::{DriverError, DriverResult};
use driver_os_api::IrqResource;
use kdevice::ProbeContext;
use virtio::Transport;

use super::{arm_pci_intx, glue::VirtIoHalImpl};
use crate::resource::DeviceResourceExt;

pub(super) fn activate_pci<T: Transport + 'static>(
    ctx: &ProbeContext<'_>,
    transport: T,
    irq: IrqResource,
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
) -> DriverResult<()> {
    activate(ctx, transport, irq, Some((config, bdf)))
}

pub(super) fn activate_mmio<T: Transport + 'static>(
    ctx: &ProbeContext<'_>,
    transport: T,
    irq: Option<IrqResource>,
) -> DriverResult<()> {
    activate(ctx, transport, irq.ok_or(DriverError::InvalidInput)?, None)
}

fn activate<T: Transport + 'static>(
    ctx: &ProbeContext<'_>,
    transport: T,
    irq: IrqResource,
    pci_intx: Option<(&mut pci::PciConfigAccess, pci::DeviceFunction)>,
) -> DriverResult<()> {
    let input = virtio::VirtIoInputDev::<VirtIoHalImpl, T>::try_new(transport, irq.virq().raw())?;
    ctx.devm_request_irq(irq, input.irq_handler())?;

    if let Some((config, bdf)) = pci_intx {
        arm_pci_intx(ctx, config, bdf)?;
    }

    kclass::publish_input(ctx, Box::new(input)).map(drop)
}

unittest::path_mod_test!(
    virtio_input_unittest,
    "../../../ut-tests/virtio_input_unittest.rs"
);
