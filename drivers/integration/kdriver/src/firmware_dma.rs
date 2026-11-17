// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Compose firmware DMA facts into one device DMA view.
//!
//! Firmware interpretation — the DMA-parent walk, ancestor translation,
//! `dma-ranges` and coherency inheritance, and interconnect/IOMMU rejection —
//! belongs to the firmware backend and is reached only through the neutral
//! [`FwNode`] queries. This module owns the integration policy on top of those
//! facts: the device's own DMA mask, the single-window restriction, the
//! platform coherency default and the scoped QEMU virt exception. It never
//! treats a missing `dma-ranges` as an unlimited window, never uses a firmware
//! window as a device mask, and never invents coherency.

use driver_os_api::{DmaInfo, DmaWindow};
use khal::firmware::devices::{FirmwareDevice, FwDmaError, FwDmaWindow, FwError, FwNode};

const VIRTIO_MMIO_COMPATIBLE: &str = "virtio,mmio";
/// Root `compatible` values of the QEMU machines whose virtio-mmio devices sit
/// directly in the CPU address space.
const QEMU_VIRT_ROOTS: [&str; 2] = ["linux,dummy-virt", "riscv-virtio"];

/// Why firmware and platform facts do not produce a device DMA view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DmaConfigurationError {
    /// Reading firmware itself failed; no platform fallback may erase it.
    Firmware(FwError),
    /// No nonempty DMA declaration was found on the DMA-parent path.
    MissingWindow,
    /// Neither the device nor an ancestor declares cache coherency.
    MissingCoherency,
    /// The device mask is not a contiguous set of low address bits.
    InvalidMask,
    /// The resolved window is empty, ambiguous or overflows an address space.
    InvalidWindow,
    /// The device declares multiple DMA windows.
    UnsupportedMultiWindow,
    /// The declared window cannot be translated to the CPU address space.
    UnsupportedTranslation,
    /// A firmware interconnect overrides the structural DMA parent.
    UnsupportedDmaParent,
    /// An IOMMU or IOMMU map controls this device's addresses.
    UnsupportedIommu,
    /// The node is not the scoped QEMU Arm virt MMIO device.
    UnsupportedPlatformPolicy,
    /// Firmware RAM is not one unambiguous nonempty interval.
    MissingPlatformRam,
}

/// Resolve an explicit firmware DMA view for `device`.
///
/// `mask` is the device's own address capability, supplied by its driver or
/// bus policy; it is never derived from the firmware window. Both pieces are
/// validated independently and retained as-is in the returned view.
///
/// # Errors
/// Returns firmware errors unchanged and rejects missing, malformed,
/// multi-window or untranslatable declarations instead of defaulting them.
pub(crate) fn from_firmware(
    device: &FirmwareDevice,
    mask: u64,
) -> Result<DmaInfo, DmaConfigurationError> {
    if !valid_mask(mask) {
        return Err(DmaConfigurationError::InvalidMask);
    }
    let window = device
        .node
        .dma_window()
        .map_err(map_dma_window_error)?
        .ok_or(DmaConfigurationError::MissingWindow)?;
    let coherent = device
        .node
        .dma_effective_coherency()
        .map_err(DmaConfigurationError::Firmware)?
        .ok_or(DmaConfigurationError::MissingCoherency)?;
    build(mask, window, coherent)
}

/// Apply the explicit QEMU virt direct-MMIO platform policy if needed.
///
/// Firmware facts and errors take precedence. Only an available `virtio,mmio`
/// device in a recognized QEMU `virt` machine may substitute the single
/// firmware-described RAM interval when no DMA window is declared, or use the
/// architecture's default coherency when no `dma-coherent` declaration exists.
/// These are machine-scoped platform facts, not a fallback for a missing
/// property in general: the same absences on any other platform remain errors.
/// The caller's device mask stays independent; this policy does not authorize
/// DMA outside that RAM interval.
///
/// # Errors
/// Returns [`from_firmware`] results unchanged, or rejects an unrelated
/// topology or ambiguous RAM description.
pub(crate) fn from_firmware_with_qemu_virt_mmio_policy(
    device: &FirmwareDevice,
    mask: u64,
) -> Result<DmaInfo, DmaConfigurationError> {
    match from_firmware(device, mask) {
        Ok(info) => return Ok(info),
        Err(DmaConfigurationError::MissingWindow | DmaConfigurationError::MissingCoherency) => {}
        Err(error) => return Err(error),
    }

    let window = device.node.dma_window().map_err(map_dma_window_error)?;
    qemu_virt_mmio_scope(device.node)?;
    let window = match window {
        Some(window) => window,
        None => qemu_virt_mmio_window(device.node)?,
    };
    let coherent = match device
        .node
        .dma_effective_coherency()
        .map_err(DmaConfigurationError::Firmware)?
    {
        Some(coherent) => coherent,
        None => qemu_virt_default_coherency()?,
    };
    build(mask, window, coherent)
}

fn map_dma_window_error(error: FwDmaError) -> DmaConfigurationError {
    match error {
        FwDmaError::Firmware(error) => DmaConfigurationError::Firmware(error),
        FwDmaError::MultiWindow => DmaConfigurationError::UnsupportedMultiWindow,
        FwDmaError::UnsupportedParent => DmaConfigurationError::UnsupportedDmaParent,
        FwDmaError::Iommu => DmaConfigurationError::UnsupportedIommu,
        FwDmaError::UnsupportedTranslation => DmaConfigurationError::UnsupportedTranslation,
        FwDmaError::InvalidWindow => DmaConfigurationError::InvalidWindow,
    }
}

/// Coherency default for the scoped QEMU machine.
///
/// Linux selects `CONFIG_ARCH_DMA_DEFAULT_COHERENT` per architecture and falls
/// back to it in `of_dma_is_coherent()`. RISC-V selects it, so a QEMU
/// `riscv-virtio` device with no declaration is coherent. Arm64 does not, so
/// the same absence remains [`DmaConfigurationError::MissingCoherency`] there.
fn qemu_virt_default_coherency() -> Result<bool, DmaConfigurationError> {
    if cfg!(target_arch = "riscv64") {
        Ok(true)
    } else {
        Err(DmaConfigurationError::MissingCoherency)
    }
}

fn build(mask: u64, window: FwDmaWindow, coherent: bool) -> Result<DmaInfo, DmaConfigurationError> {
    let window = DmaWindow::new(window.cpu_addr, window.bus_addr, window.size)
        .map_err(|_| DmaConfigurationError::InvalidWindow)?;
    DmaInfo::new(mask, window, coherent).map_err(|_| DmaConfigurationError::InvalidMask)
}

const fn valid_mask(mask: u64) -> bool {
    mask & mask.wrapping_add(1) == 0
}

/// Restrict the platform policy to a QEMU `virt` machine.
///
/// Arm places `virtio,mmio` nodes directly under the machine root; RISC-V
/// places them under a `simple-bus` `soc` container. Both are accepted, and
/// nothing else between the device and the root is: the chain may only cross
/// available `simple-bus` nodes, and the root itself must be recognizable and
/// have no parent.
fn qemu_virt_mmio_scope(node: FwNode<'_>) -> Result<(), DmaConfigurationError> {
    if !cfg!(any(target_arch = "aarch64", target_arch = "riscv64"))
        || !node.firmware_ids().any(|id| id == VIRTIO_MMIO_COMPATIBLE)
        || !node
            .is_available()
            .map_err(DmaConfigurationError::Firmware)?
    {
        return Err(DmaConfigurationError::UnsupportedPlatformPolicy);
    }

    let mut current = node
        .parent()
        .ok_or(DmaConfigurationError::UnsupportedPlatformPolicy)?;
    loop {
        if !current
            .is_available()
            .map_err(DmaConfigurationError::Firmware)?
        {
            return Err(DmaConfigurationError::UnsupportedPlatformPolicy);
        }
        if current
            .firmware_ids()
            .any(|id| QEMU_VIRT_ROOTS.contains(&id))
        {
            return if current.parent().is_none() {
                Ok(())
            } else {
                Err(DmaConfigurationError::UnsupportedPlatformPolicy)
            };
        }
        let parent = current
            .parent()
            .ok_or(DmaConfigurationError::UnsupportedPlatformPolicy)?;
        if !current.firmware_ids().any(|id| id == "simple-bus") {
            return Err(DmaConfigurationError::UnsupportedPlatformPolicy);
        }
        current = parent;
    }
}

fn qemu_virt_mmio_window(node: FwNode<'_>) -> Result<FwDmaWindow, DmaConfigurationError> {
    qemu_virt_mmio_scope(node)?;
    let ram = khal::mem::boot_ram_regions().ok_or(DmaConfigurationError::MissingPlatformRam)?;
    let [(start, size)] = ram else {
        return Err(DmaConfigurationError::MissingPlatformRam);
    };
    if *size == 0 {
        return Err(DmaConfigurationError::MissingPlatformRam);
    }
    let start = u64::try_from(*start).map_err(|_| DmaConfigurationError::InvalidWindow)?;
    let size = u64::try_from(*size).map_err(|_| DmaConfigurationError::InvalidWindow)?;
    Ok(FwDmaWindow {
        bus_addr: start,
        cpu_addr: start,
        size,
    })
}

unittest::path_mod_test!(tests, "../ut-tests/firmware_dma.rs");
