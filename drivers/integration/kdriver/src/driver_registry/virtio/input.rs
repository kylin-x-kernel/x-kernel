// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO input activation and host interrupt-resource ownership.

use alloc::{boxed::Box, sync::Arc};

use device_res::{IrqResource, IrqTrigger};
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;
use virtio::Transport;

use super::glue::VirtIoHalImpl;
use crate::resource::DeviceResourceExt;

pub(super) fn activate_pci<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: usize,
    config: &mut pci::PciConfigAccess,
    bdf: pci::DeviceFunction,
) -> DriverResult<()> {
    activate(parent, transport, irq, Some((config, bdf)))
}

pub(super) fn activate_mmio<T: Transport + 'static>(
    parent: Arc<DeviceObject>,
    transport: T,
    irq: Option<usize>,
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
    irq: usize,
    pci_intx: Option<(&mut pci::PciConfigAccess, pci::DeviceFunction)>,
) -> DriverResult<()> {
    let input = virtio::VirtIoInputDev::<VirtIoHalImpl, T>::try_new(transport, irq)?;
    parent.devm_request_irq(
        IrqResource::new(irq, IrqTrigger::Unknown(0)),
        input.irq_handler(),
    )?;

    if let Some((config, bdf)) = pci_intx {
        // The transport stays masked until the shared handler is registered.
        // LIFO cleanup masks INTx before the IRQ action is released.
        let mut cleanup_config = *config;
        parent.add_cleanup(move || set_pci_intx_disabled(&mut cleanup_config, bdf, true));
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

#[cfg(unittest)]
mod tests {
    use pci::{Cam, DeviceFunction, PciConfigAccess};
    use unittest::{assert_eq, def_test};

    use super::set_pci_intx_disabled;

    fn config_for_command(command: u32) -> ([u32; 64], DeviceFunction) {
        let mut config_space = [0u32; 64];
        config_space[1] = command;
        let bdf = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        (config_space, bdf)
    }

    #[def_test]
    fn input_cleanup_masks_intx_without_writing_pci_status() {
        let (mut config_space, bdf) = config_for_command(0xabcd_8807);
        // SAFETY: the aligned test array covers function 0's command register
        // and remains alive for every access through `config`.
        let mut config =
            unsafe { PciConfigAccess::new(config_space.as_mut_ptr().cast::<u8>(), Cam::MmioCam) };

        set_pci_intx_disabled(&mut config, bdf, true);

        assert_eq!(config_space[1], 0xabcd_8c07);
    }

    #[def_test]
    fn input_activation_unmasks_intx_without_writing_pci_status() {
        let (mut config_space, bdf) = config_for_command(0xabcd_8c07);
        // SAFETY: the aligned test array covers function 0's command register
        // and remains alive for every access through `config`.
        let mut config =
            unsafe { PciConfigAccess::new(config_space.as_mut_ptr().cast::<u8>(), Cam::MmioCam) };

        set_pci_intx_disabled(&mut config, bdf, false);

        assert_eq!(config_space[1], 0xabcd_8807);
    }
}
