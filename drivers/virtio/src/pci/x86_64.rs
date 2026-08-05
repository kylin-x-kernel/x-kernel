// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use device_res::{
    IrqController, IrqRouteDesc, IrqTrigger, MsiResource, irq_provider, try_irq_provider,
};
use pci::{
    PciConfigAccess,
    msix::{self, MsixCapability, MsixTable, MsixTableEntry, PCI_BAR_COUNT},
};
use safe_mmio::{UniqueMmioPointer, field, field_shared, fields::ReadPureWrite};
use virtio_drivers::{
    PhysAddr,
    transport::pci::bus::{Command, ConfigurationAccess, DeviceFunction, PciRoot},
};

use crate::VirtIoHal;

impl Drop for super::PciTransport {
    fn drop(&mut self) {
        self.irq_state.release();
    }
}

pub(super) struct PciMsixState {
    common_cfg: VirtioPciCommonCfg,
    config: PciConfigAccess,
    cap: MsixCapability,
    bdf: DeviceFunction,
    msi: MsiResource,
}

pub(super) enum PciIrqState {
    Legacy,
    Msix(PciMsixState),
    Failed,
}

impl PciIrqState {
    pub(super) fn new(msix: Option<PciMsixState>) -> Self {
        match msix {
            Some(msix) => Self::Msix(msix),
            None => Self::Legacy,
        }
    }

    pub(super) fn is_failed(&self) -> bool {
        matches!(self, Self::Failed)
    }

    pub(super) fn msix_mut(&mut self) -> Option<&mut PciMsixState> {
        match self {
            Self::Msix(msix) => Some(msix),
            Self::Legacy | Self::Failed => None,
        }
    }

    pub(super) fn fail(&mut self) {
        if !matches!(self, Self::Msix(_)) {
            return;
        }

        let Self::Msix(msix) = core::mem::replace(self, Self::Failed) else {
            unreachable!();
        };
        msix.release();
    }

    pub(super) fn release(&mut self) {
        if !matches!(self, Self::Msix(_)) {
            return;
        }

        let Self::Msix(msix) = core::mem::replace(self, Self::Legacy) else {
            unreachable!();
        };
        msix.release();
    }
}

impl PciMsixState {
    const CONFIG_TABLE_ENTRY: u16 = 0;
    const NO_VECTOR: u16 = 0xffff;
    const QUEUE_TABLE_ENTRY: u16 = 1;

    pub(super) fn bdf(&self) -> DeviceFunction {
        self.bdf
    }

    pub(super) fn set_config_vector(&mut self) -> bool {
        self.write_msix_vector(
            CommonCfgMsixVector::Config,
            Self::CONFIG_TABLE_ENTRY,
            "config",
        )
    }

    pub(super) fn set_queue_vector(&mut self, queue: u16) -> bool {
        self.common_cfg.select_queue(queue);
        // Read back `queue_select` to flush the posted MMIO write before
        // programming `queue_msix_vector` for the selected queue.
        let _ = self.common_cfg.selected_queue();
        self.write_msix_vector(CommonCfgMsixVector::Queue, Self::QUEUE_TABLE_ENTRY, "queue")
    }

    pub(super) fn activate(&self) {
        let mut config = self.config;
        msix::activate_msix(&mut config, self.bdf, &self.cap);
    }

    pub(super) fn release(self) {
        self.disable();
        if let Some(p) = try_irq_provider() {
            p.free_msix(self.msi);
        }
    }

    fn disable(&self) {
        let mut config = self.config;
        msix::disable_msix_with_config(&mut config, self.bdf, &self.cap);
    }

    fn write_msix_vector(
        &mut self,
        field: CommonCfgMsixVector,
        table_entry: u16,
        field_name: &str,
    ) -> bool {
        let selected_entry = match field {
            CommonCfgMsixVector::Config => self.common_cfg.set_config_msix_vector(table_entry),
            CommonCfgMsixVector::Queue => self.common_cfg.set_queue_msix_vector(table_entry),
        };
        if selected_entry == table_entry {
            return true;
        }

        let reason = if selected_entry == Self::NO_VECTOR {
            "device rejected the vector"
        } else {
            "device selected a different vector"
        };
        log::warn!(
            "PCI virtio device at {:?}: MSI-X {} vector {} not accepted, read back {:#x}: {}",
            self.bdf,
            field_name,
            table_entry,
            selected_entry,
            reason
        );
        false
    }
}

struct VirtioPciCommonCfg {
    regs: UniqueMmioPointer<'static, VirtioPciCommonCfgRegs>,
}

#[derive(Clone, Copy)]
enum CommonCfgMsixVector {
    Config,
    Queue,
}

impl VirtioPciCommonCfg {
    const MIN_LEN: u32 = (core::mem::offset_of!(VirtioPciCommonCfgRegs, queue_msix_vector) as u32)
        + core::mem::size_of::<u16>() as u32;

    fn new(base: core::ptr::NonNull<u8>) -> Self {
        // SAFETY: `base` is a validated VirtIO PCI common configuration MMIO
        // mapping. `map_virtio_common_cfg` checks the region length and
        // alignment before creating this wrapper.
        let regs = unsafe { UniqueMmioPointer::new(base.cast()) };
        Self { regs }
    }

    fn select_queue(&mut self, queue: u16) {
        field!(self.regs, queue_select).write(queue);
    }

    fn selected_queue(&self) -> u16 {
        field_shared!(self.regs, queue_select).read()
    }

    fn set_config_msix_vector(&mut self, table_entry: u16) -> u16 {
        let mut field = field!(self.regs, msix_config);
        field.write(table_entry);
        field.read()
    }

    fn set_queue_msix_vector(&mut self, table_entry: u16) -> u16 {
        let mut field = field!(self.regs, queue_msix_vector);
        field.write(table_entry);
        field.read()
    }
}

#[repr(C)]
struct VirtioPciCommonCfgRegs {
    _device_feature_select: u32,
    _device_feature: u32,
    _driver_feature_select: u32,
    _driver_feature: u32,
    msix_config: ReadPureWrite<u16>,
    _num_queues: u16,
    _device_status: u8,
    _config_generation: u8,
    queue_select: ReadPureWrite<u16>,
    _queue_size: u16,
    queue_msix_vector: ReadPureWrite<u16>,
}

#[derive(Clone, Copy)]
struct VirtioPciRegion {
    bar: u8,
    offset: u32,
    length: u32,
}

fn find_virtio_common_cfg<C: ConfigurationAccess>(
    root: &PciRoot<C>,
    bdf: DeviceFunction,
    config: &PciConfigAccess,
) -> Option<VirtioPciRegion> {
    const PCI_CAP_ID_VNDR: u8 = 0x09;
    const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
    const CAP_BAR_OFFSET: u16 = 4;
    const CAP_BAR_OFFSET_OFFSET: u16 = 8;
    const CAP_LENGTH_OFFSET: u16 = 12;

    for capability in root.capabilities(bdf) {
        if capability.id != PCI_CAP_ID_VNDR {
            continue;
        }

        let cap_len = capability.private_header as u8;
        let cfg_type = (capability.private_header >> 8) as u8;
        if cap_len < 16 || cfg_type != VIRTIO_PCI_CAP_COMMON_CFG {
            continue;
        }

        let cap_offset = capability.offset as u16;
        let bar = (config.read_word(bdf, cap_offset + CAP_BAR_OFFSET) & 0xff) as u8;
        let offset = config.read_word(bdf, cap_offset + CAP_BAR_OFFSET_OFFSET);
        let length = config.read_word(bdf, cap_offset + CAP_LENGTH_OFFSET);

        return Some(VirtioPciRegion {
            bar,
            offset,
            length,
        });
    }

    None
}

fn map_virtio_common_cfg<H: VirtIoHal, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    config: &PciConfigAccess,
) -> Option<VirtioPciCommonCfg> {
    let region = find_virtio_common_cfg(root, bdf, config)?;
    if region.length < VirtioPciCommonCfg::MIN_LEN {
        log::warn!(
            "PCI virtio device at {:?}: common cfg too small: {} bytes",
            bdf,
            region.length
        );
        return None;
    }

    let bar_info = match root.bar_info(bdf, region.bar) {
        Ok(Some(info)) => info,
        Ok(None) | Err(_) => return None,
    };
    let (bar_address, bar_size) = bar_info.memory_address_size()?;
    let region_end = u64::from(region.offset).checked_add(u64::from(region.length))?;
    if region_end > bar_size {
        log::warn!(
            "PCI virtio device at {:?}: common cfg outside BAR{}",
            bdf,
            region.bar
        );
        return None;
    }

    let paddr = bar_address as PhysAddr + region.offset as PhysAddr;
    // SAFETY: The BAR information comes from PCI configuration space and the
    // offset/length bounds were checked against the BAR size above.
    let common_cfg = unsafe { H::mmio_phys_to_virt(paddr, region.length as usize) };
    if !(common_cfg.as_ptr() as usize)
        .is_multiple_of(core::mem::align_of::<VirtioPciCommonCfgRegs>())
    {
        log::warn!(
            "PCI virtio device at {:?}: common cfg misaligned at {:#x}",
            bdf,
            common_cfg.as_ptr() as usize
        );
        return None;
    }

    Some(VirtioPciCommonCfg::new(common_cfg))
}

pub(super) fn setup_msix<H: VirtIoHal, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    config: &mut PciConfigAccess,
) -> Option<(usize, PciMsixState)> {
    use core::mem::size_of;

    const REQUIRED_MSIX_TABLE_ENTRIES: u16 = 2;

    let cap = msix::find_msix_capability(root, config, bdf)?;
    if cap.table_bar >= PCI_BAR_COUNT || cap.pba_bar >= PCI_BAR_COUNT {
        log::warn!(
            "PCI virtio device at {:?}: invalid MSI-X BAR index: table={} pba={}",
            bdf,
            cap.table_bar,
            cap.pba_bar
        );
        return None;
    }
    if cap.table_size < REQUIRED_MSIX_TABLE_ENTRIES {
        log::warn!(
            "PCI virtio device at {:?}: MSI-X table size {} is smaller than required {}",
            bdf,
            cap.table_size,
            REQUIRED_MSIX_TABLE_ENTRIES
        );
        return None;
    }

    let (_, command) = root.get_status_command(bdf);
    root.set_command(bdf, command | Command::MEMORY_SPACE | Command::BUS_MASTER);

    // The IRQ provider is installed once during early init and never removed,
    // so the same reference serves allocation and release for the MSI-X
    // resource — no need to re-query or fall back to a default.
    let provider = match irq_provider() {
        Ok(p) => p,
        Err(_) => {
            root.set_command(bdf, command);
            return None;
        }
    };

    let prepared = (|| {
        let common_cfg = map_virtio_common_cfg::<H, C>(root, bdf, config)?;

        let bar_info = match root.bar_info(bdf, cap.table_bar) {
            Ok(Some(info)) => info,
            Ok(None) | Err(_) => return None,
        };
        let (bar_address, bar_size) = bar_info.memory_address_size()?;
        let pba_bar_info = match root.bar_info(bdf, cap.pba_bar) {
            Ok(Some(info)) => info,
            Ok(None) | Err(_) => return None,
        };
        let (_, pba_bar_size) = pba_bar_info.memory_address_size()?;
        if let Err(reason) = msix::validate_msix_layout(&cap, bar_size, pba_bar_size) {
            log::warn!(
                "PCI virtio device at {:?}: invalid MSI-X layout: {}",
                bdf,
                reason
            );
            return None;
        }

        let table_bytes = usize::from(cap.table_size).checked_mul(size_of::<MsixTableEntry>())?;
        let table_paddr = bar_address as PhysAddr + cap.table_offset as PhysAddr;
        // SAFETY: The MSI-X table BAR, offset, and size were validated above.
        let table_base =
            unsafe { H::mmio_phys_to_virt(table_paddr, table_bytes) }.cast::<MsixTableEntry>();
        // SAFETY: `table_base` points at the mapped MSI-X table. The table
        // size was validated against the BAR size above.
        let table = unsafe { MsixTable::new(table_base, usize::from(cap.table_size)) };

        let msi = provider.alloc_msix().ok()?;
        Some((common_cfg, table, msi))
    })();
    let Some((common_cfg, table, msi)) = prepared else {
        root.set_command(bdf, command);
        return None;
    };

    msix::prepare_msix(root, config, bdf, &cap);

    for entry in 0..usize::from(REQUIRED_MSIX_TABLE_ENTRIES) {
        if msix::configure_msix_entry(&table, entry, msi.message.address, msi.message.data)
            .is_some()
        {
            continue;
        }

        log::warn!(
            "PCI virtio device at {:?}: MSI-X table entry {} is unavailable",
            bdf,
            entry
        );
        msix::disable_msix(root, config, bdf, &cap);
        provider.free_msix(msi);
        root.set_command(bdf, command);
        return None;
    }

    let irq = msi.irq.number;
    let msix = PciMsixState {
        common_cfg,
        config: *config,
        cap,
        bdf,
        msi,
    };

    log::info!("PCI virtio device at {:?}: MSI-X IRQ = {}", bdf, irq);

    Some((irq, msix))
}

/// Reads the PCI Interrupt Line register (config space offset 0x3C) for the
/// given device and returns it as a legacy IRQ number.
///
/// Returns 0xFF if the register has not been programmed by firmware, which
/// means the device has no usable legacy IRQ assignment. The caller should
/// treat 0xFF as "no IRQ".
pub(super) fn legacy_irq_for_bdf(config: &PciConfigAccess, bdf: DeviceFunction) -> Option<usize> {
    let word = config.read_word(bdf, 0x3C);
    let irq_line = (word & 0xFF) as usize;
    if irq_line == 0xFF || irq_line == 0 {
        log::warn!(
            "PCI device {:?}: Interrupt Line not assigned ({:#x}), legacy IRQ unavailable",
            bdf,
            irq_line
        );
        return None;
    }

    let desc = IrqRouteDesc {
        hwirq: irq_line,
        trigger: IrqTrigger::LevelLow,
        controller: IrqController::IoApic,
        domain: None,
    };
    Some(irq_provider().ok()?.map_irq(desc).ok()?.number)
}
