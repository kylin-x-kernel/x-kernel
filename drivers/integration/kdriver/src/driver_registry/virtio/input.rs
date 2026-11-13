// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input activation and host interrupt-resource ownership.

use alloc::{boxed::Box, sync::Arc};

use driver_base::{DriverError, DriverResult};
use driver_os_api::IrqResource;
use kdevice::DeviceObject;
use virtio::Transport;

use super::glue::VirtIoHalImpl;
use crate::resource::DeviceResourceExt;

pub(super) fn activate_pci<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: IrqResource,
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
) -> DriverResult<()> {
    activate(parent, transport, irq, Some((config, bdf)))
}

pub(super) fn activate_mmio<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: Option<IrqResource>,
) -> DriverResult<()> {
    activate(
        parent,
        transport,
        irq.ok_or(DriverError::InvalidInput)?,
        None,
    )
}

fn activate<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: IrqResource,
    pci_intx: Option<(&mut pci::PciConfigAccess, pci::DeviceFunction)>,
) -> DriverResult<()> {
    let binding = parent
        .binding_ref_for_lifecycle_write()
        .ok_or(DriverError::BadState)?;
    let input = virtio::VirtIoInputDev::<VirtIoHalImpl, T>::try_new(transport, irq.virq().raw())?;
    binding.devm_request_irq(&parent, irq, input.irq_handler())?;

    if let Some((config, bdf)) = pci_intx {
        // The transport stays masked until the shared handler is registered.
        // LIFO cleanup masks INTx before the IRQ action is released.
        let mut cleanup_config = *config;
        binding.add_cleanup(&parent, move || {
            set_pci_intx_disabled(&mut cleanup_config, bdf, true)
        })?;
        set_pci_intx_disabled(config, bdf, false);
    }

    kclass::publish_input(parent, Box::new(input)).map(drop)
}

fn set_pci_intx_disabled(
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
    is_disabled: bool,
) {
    const INTERRUPT_DISABLE: u16 = 1 << 10;
    const STATUS_COMMAND_OFFSET: u16 = 0x04;

    let status_command = config.read_word(bdf, STATUS_COMMAND_OFFSET);
    let command = if is_disabled {
        status_command as u16 | INTERRUPT_DISABLE
    } else {
        status_command as u16 & !INTERRUPT_DISABLE
    };
    // Status has write-one-to-clear bits in the same 32-bit word. Only write
    // the Command halfword so a pending Status bit is not cleared here.
    config.write_halfword(bdf, STATUS_COMMAND_OFFSET, command);
}

unittest::path_mod_test!(
    virtio_input_unittest,
    "../../../ut-tests/virtio_input_unittest.rs"
);
