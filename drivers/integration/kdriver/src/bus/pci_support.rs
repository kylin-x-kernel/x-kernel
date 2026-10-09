// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared PCI helper routines used by the kdriver bus and activation paths.

use driver_base::{DriverError, DriverResult};
use kspin::{SpinNoPreempt, SpinNoPreemptGuard};
use lazyinit::LazyInit;

/// Lazily-initialised allocator for unassigned PCI memory BARs.
///
/// `LazyInit::call_once` runs the constructor *outside* of any spinlock
/// guard, so it is safe for the constructor to walk firmware tables (which
/// may take their own locks). BAR assignment only happens during driver
/// probe / bus enumeration in process context, so a [`SpinNoPreempt`] is
/// sufficient to serialise access to the shared allocation cursor.
static PCI_BAR_ALLOCATOR: LazyInit<SpinNoPreempt<Option<pci::PciRangeAllocator>>> = LazyInit::new();

fn pci_bar_allocator() -> SpinNoPreemptGuard<'static, Option<pci::PciRangeAllocator>> {
    PCI_BAR_ALLOCATOR.call_once(|| {
        let alloc = pci::pci_bar_allocation_range()
            .map(|(start, len, _)| pci::PciRangeAllocator::new(start, len));
        SpinNoPreempt::new(alloc)
    });
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: call_once completed successfully and \
                  permanently published the PCI BAR allocator slot"
    )]
    PCI_BAR_ALLOCATOR
        .get()
        .expect("PCI_BAR_ALLOCATOR call_once succeeded")
        .lock()
}

pub(crate) fn pci_cam_kind() -> pci::Cam {
    #[cfg(feature = "pci-non-ecam")]
    {
        pci::Cam::MmioCam
    }

    #[cfg(not(feature = "pci-non-ecam"))]
    {
        pci::Cam::Ecam
    }
}

fn has_unassigned_memory_bar<C: pci::ConfigurationAccess>(
    root: &mut pci::PciRoot<C>,
    bdf: pci::DeviceFunction,
) -> bool {
    let mut bar_idx = 0u8;
    while bar_idx < 6 {
        let info = match root.bar_info(bdf, bar_idx) {
            Ok(Some(info)) => info,
            _ => {
                bar_idx += 1;
                continue;
            }
        };

        let advance = if info.takes_two_entries() { 2 } else { 1 };
        if matches!(
            info,
            pci::BarInfo::Memory {
                address: 0,
                size,
                ..
            } if size > 0
        ) {
            return true;
        }

        bar_idx += advance;
    }

    false
}

/// Allocate unassigned PCI memory BARs if needed, then enable runtime access.
///
/// If any memory BAR is still unassigned, this function allocates an address
/// from the shared PCI MMIO window and writes it back into config space. It
/// then enables the standard PCI command bits (`IO_SPACE`, `MEMORY_SPACE`,
/// `BUS_MASTER`) expected by runtime drivers.
///
/// If all memory BARs are already assigned, the shared allocator is not
/// touched; the call only refreshes the device command bits. If any memory BAR
/// is unassigned and no allocation range is available, returns
/// [`DriverError::NoMemory`].
pub(crate) fn configure_pci_device_if_needed<C: pci::ConfigurationAccess>(
    root: &mut pci::PciRoot<C>,
    bdf: pci::DeviceFunction,
) -> DriverResult {
    let any_unassigned_memory_bar = has_unassigned_memory_bar(root, bdf);
    if !any_unassigned_memory_bar {
        let mut no_allocator = None;
        return pci::configure_device(root, bdf, &mut no_allocator).map_err(|err| {
            warn!("failed to configure PCI device {:?}: {:?}", bdf, err);
            match err {
                pci::PciInitError::NoMemory => DriverError::NoMemory,
                pci::PciInitError::InvalidRange | pci::PciInitError::InvalidBar => {
                    DriverError::InvalidInput
                }
                pci::PciInitError::MappingFailed => DriverError::Io,
            }
        });
    }

    let mut allocator = pci_bar_allocator();
    if allocator.is_none() {
        warn!(
            "PCI device {:?} has unassigned memory BARs but no allocation range",
            bdf
        );
        return Err(DriverError::NoMemory);
    }

    pci::configure_device(root, bdf, &mut allocator).map_err(|err| {
        warn!("failed to configure PCI device {:?}: {:?}", bdf, err);
        match err {
            pci::PciInitError::NoMemory => DriverError::NoMemory,
            pci::PciInitError::InvalidRange | pci::PciInitError::InvalidBar => {
                DriverError::InvalidInput
            }
            pci::PciInitError::MappingFailed => DriverError::Io,
        }
    })
}

#[unittest::mod_test]
mod tests {
    use pci::{ConfigurationAccess, DeviceFunction, PciInitError, PciRoot, configure_device};

    use super::*;

    struct InvalidBarConfig;

    impl ConfigurationAccess for InvalidBarConfig {
        fn read_word(&self, _bdf: DeviceFunction, offset: u8) -> u32 {
            if offset == 0x10 { 0b110 } else { 0 }
        }

        fn write_word(&mut self, _bdf: DeviceFunction, _offset: u8, _value: u32) {}

        unsafe fn unsafe_clone(&self) -> Self {
            Self
        }
    }

    #[unittest::def_test]
    fn invalid_bar_encoding_returns_error() {
        let mut root = PciRoot::new(InvalidBarConfig);
        let bdf = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        unittest::assert_eq!(
            configure_device(&mut root, bdf, &mut None),
            Err(PciInitError::InvalidBar)
        );
        unittest::assert_eq!(
            configure_pci_device_if_needed(&mut root, bdf),
            Err(DriverError::InvalidInput)
        );
    }
}
