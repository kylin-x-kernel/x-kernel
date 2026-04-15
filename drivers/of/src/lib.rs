// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree access helpers.

#![no_std]

use lazyinit::LazyInit;
pub use rs_fdtree::{
    Chosen, Dice, FdtError, FdtNode, InterruptController, LinuxFdt, MemoryRegion, NodeProperty,
    RegIter,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirmwareInitError {
    MissingDeviceTreePtr,
    BadDeviceTree(FdtError),
}

static FDT: LazyInit<LinuxFdt<'static>> = LazyInit::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptTrigger {
    EdgeRising,
    EdgeFalling,
    LevelHigh,
    LevelLow,
    Unknown(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptControllerKind {
    Gic,
    Plic,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciHostCam {
    Cam,
    Ecam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciHostInfo {
    pub cam: PciHostCam,
    pub ecam_base: u64,
    pub ecam_size: u64,
    pub bus_start: u8,
    pub bus_end: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciRangeInfo {
    pub cpu_base: u64,
    pub size: u64,
    pub prefetchable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptInfo {
    pub irq: usize,
    pub trigger: InterruptTrigger,
    pub controller: InterruptControllerKind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NamedMemoryRegion {
    pub region: crate::MemoryRegion,
    pub name: &'static str,
}

fn property_u32_cells<const N: usize>(
    node: FdtNode<'static, 'static>,
    name: &str,
) -> Option<[u32; N]> {
    let value = node.property(name)?.value;
    if value.len() < N * 4 {
        return None;
    }

    let mut cells = [0u32; N];
    for (idx, chunk) in value.chunks_exact(4).take(N).enumerate() {
        cells[idx] = u32::from_be_bytes(chunk.try_into().ok()?);
    }
    Some(cells)
}

pub fn property_u32(node: FdtNode<'static, 'static>, name: &str) -> Option<u32> {
    node.property_u32(name)
}

fn parse_cells_u64(cells: &[u32]) -> Option<u64> {
    match cells {
        [value] => Some(*value as u64),
        [hi, lo] => Some(((*hi as u64) << 32) | (*lo as u64)),
        _ => None,
    }
}

fn parse_gic_trigger(flags: u32) -> InterruptTrigger {
    match flags & 0xf {
        1 => InterruptTrigger::EdgeRising,
        2 => InterruptTrigger::EdgeFalling,
        4 => InterruptTrigger::LevelHigh,
        8 => InterruptTrigger::LevelLow,
        other => InterruptTrigger::Unknown(other),
    }
}

fn parse_gic_interrupt(cells: &[u32]) -> Option<InterruptInfo> {
    match *cells {
        [0, irq, flags, ..] => Some(InterruptInfo {
            irq: 32 + irq as usize,
            trigger: parse_gic_trigger(flags),
            controller: InterruptControllerKind::Gic,
        }),
        [1, irq, flags, ..] => Some(InterruptInfo {
            irq: 16 + irq as usize,
            trigger: parse_gic_trigger(flags),
            controller: InterruptControllerKind::Gic,
        }),
        [irq, ..] => Some(InterruptInfo {
            irq: irq as usize,
            trigger: InterruptTrigger::Unknown(0),
            controller: InterruptControllerKind::Gic,
        }),
        _ => None,
    }
}

fn parse_plic_interrupt(cells: &[u32]) -> Option<InterruptInfo> {
    match *cells {
        [irq, ..] => Some(InterruptInfo {
            irq: irq as usize,
            trigger: InterruptTrigger::Unknown(0),
            controller: InterruptControllerKind::Plic,
        }),
        _ => None,
    }
}

fn controller_kind(node: FdtNode<'static, 'static>) -> InterruptControllerKind {
    if node.compatibles().any(|compatible| {
        matches!(
            compatible,
            "arm,gic-400"
                | "arm,cortex-a15-gic"
                | "arm,cortex-a7-gic"
                | "arm,gic-v2"
                | "arm,gic-v3"
                | "arm,gic-v4"
        )
    }) {
        InterruptControllerKind::Gic
    } else if node
        .compatibles()
        .any(|compatible| matches!(compatible, "sifive,plic-1.0.0" | "riscv,plic0"))
    {
        InterruptControllerKind::Plic
    } else {
        InterruptControllerKind::Unknown
    }
}

fn parse_interrupt_by_controller(
    controller: InterruptControllerKind,
    cells: &[u32],
) -> Option<InterruptInfo> {
    match controller {
        InterruptControllerKind::Gic => parse_gic_interrupt(cells),
        InterruptControllerKind::Plic => parse_plic_interrupt(cells),
        _ => None,
    }
}

fn find_node_by_phandle(phandle: u32) -> Option<FdtNode<'static, 'static>> {
    fdt()?.all_nodes().find(|node| {
        property_u32(*node, "phandle").or_else(|| property_u32(*node, "linux,phandle"))
            == Some(phandle)
    })
}

fn interrupt_parent_node(node: FdtNode<'static, 'static>) -> Option<FdtNode<'static, 'static>> {
    let phandle = property_u32(node, "interrupt-parent").or_else(|| {
        node.parent_property("interrupt-parent")
            .and_then(|prop| prop.value.get(..4))
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
    })?;
    find_node_by_phandle(phandle)
}

pub fn fdt() -> Option<&'static LinuxFdt<'static>> {
    FDT.get()
}

/// Read the total size of the DTB referenced by `ptr`.
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn dtb_total_size_from_ptr(ptr: *const u8) -> Result<usize, FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(fdt.total_size())
}

pub fn dtb_total_size() -> Option<usize> {
    Some(fdt()?.total_size())
}

/// Initialize the global DTB handle from a raw pointer.
///
/// # Safety
///
/// `ptr` must point to a valid DTB blob that remains accessible for the rest of
/// the program lifetime.
pub unsafe fn init_device_tree_ptr(ptr: *const u8) -> Result<(), FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    FDT.init_once(fdt);
    Ok(())
}

pub fn chosen_bootargs() -> Option<&'static str> {
    fdt()?.chosen_bootargs()
}

pub fn root_model() -> Option<&'static str> {
    fdt()?.root_model()
}

pub fn root_compatible() -> Option<&'static str> {
    fdt()?.root_compatible()
}

pub fn find_compatible(compatible: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.find_compatible(compatible)
}

pub fn generic_pci_host() -> Option<FdtNode<'static, 'static>> {
    find_compatible("pci-host-ecam-generic").or_else(|| find_compatible("pci-host-cam-generic"))
}

pub fn find_node(path: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.find_node(path)
}

pub fn chosen_stdout_path() -> Option<&'static str> {
    fdt()?.chosen_stdout_path()
}

pub fn chosen() -> Option<Chosen<'static, 'static>> {
    fdt()?.chosen()
}

pub fn resolve_node(path_or_alias: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.resolve_node(path_or_alias)
}

/// Returns the first interrupt specifier for a device node.
pub fn first_interrupt_desc(node: FdtNode<'static, 'static>) -> Option<InterruptInfo> {
    if let Some(parent) = interrupt_parent_node(node) {
        let controller = controller_kind(parent);
        if let Some(cells) = property_u32_cells::<3>(node, "interrupts")
            && let Some(irq) = parse_interrupt_by_controller(controller, &cells)
        {
            return Some(irq);
        }

        if let Some(cells) = property_u32_cells::<1>(node, "interrupts")
            && let Some(irq) = parse_interrupt_by_controller(controller, &cells)
        {
            return Some(irq);
        }
    }

    if let Some(cells) = property_u32_cells::<4>(node, "interrupts-extended") {
        let controller_node = find_node_by_phandle(cells[0])?;
        let controller = controller_kind(controller_node);
        let irq = parse_interrupt_by_controller(controller, &cells[1..])?;
        return Some(irq);
    }

    if let Some(cells) = property_u32_cells::<2>(node, "interrupts-extended") {
        let controller_node = find_node_by_phandle(cells[0])?;
        let controller = controller_kind(controller_node);
        let irq = parse_interrupt_by_controller(controller, &cells[1..])?;
        return Some(irq);
    }

    None
}

pub fn generic_pci_host_info() -> Option<PciHostInfo> {
    let node = generic_pci_host()?;
    let cam = if node.is_compatible("pci-host-cam-generic") {
        PciHostCam::Cam
    } else {
        PciHostCam::Ecam
    };
    let reg = node.reg()?.next()?;
    let [bus_start, bus_end] = property_u32_cells::<2>(node, "bus-range").unwrap_or([0, 0xff]);
    Some(PciHostInfo {
        cam,
        ecam_base: reg.starting_address as usize as u64,
        ecam_size: reg.size as u64,
        bus_start: bus_start as u8,
        bus_end: bus_end as u8,
    })
}

pub fn generic_pci_non_prefetchable_mem_range() -> Option<PciRangeInfo> {
    const PCI_ADDR_SPACE_MASK: u32 = 0x0300_0000;
    const PCI_ADDR_SPACE_MEM32: u32 = 0x0200_0000;
    const PCI_ADDR_SPACE_MEM64: u32 = 0x0300_0000;
    const PCI_ADDR_PREFETCH: u32 = 0x4000_0000;

    let node = generic_pci_host()?;
    let child = node.cell_sizes();
    let parent_address_cells = node.parent_property_u32("#address-cells").unwrap_or(2) as usize;
    let total_cells = child.address_cells + parent_address_cells + child.size_cells;
    let value = node.property("ranges")?.value;
    if total_cells == 0 || value.len() < total_cells * 4 {
        return None;
    }

    let mut preferred = None;
    let mut fallback = None;
    for chunk in value.chunks_exact(total_cells * 4) {
        let mut cells = [0u32; 7];
        if total_cells > cells.len() {
            return None;
        }
        for (idx, word) in chunk.chunks_exact(4).enumerate() {
            cells[idx] = u32::from_be_bytes(word.try_into().ok()?);
        }
        let flags = cells[0];
        let space = flags & PCI_ADDR_SPACE_MASK;
        if space != PCI_ADDR_SPACE_MEM32 && space != PCI_ADDR_SPACE_MEM64 {
            continue;
        }
        let parent_start = child.address_cells;
        let parent_end = parent_start + parent_address_cells;
        let size_end = parent_end + child.size_cells;
        let cpu_base = parse_cells_u64(&cells[parent_start..parent_end])?;
        let size = parse_cells_u64(&cells[parent_end..size_end])?;
        let range = PciRangeInfo {
            cpu_base,
            size,
            prefetchable: (flags & PCI_ADDR_PREFETCH) != 0,
        };
        if !range.prefetchable {
            preferred = Some(range);
            break;
        }
        fallback = Some(range);
    }

    preferred.or(fallback)
}

pub fn generic_pci_legacy_interrupt(
    bus: u8,
    device: u8,
    function: u8,
    pin: u8,
) -> Option<InterruptInfo> {
    let node = generic_pci_host()?;
    let child_address_cells = node.cell_sizes().address_cells;
    let child_interrupt_cells = property_u32(node, "#interrupt-cells").unwrap_or(1) as usize;
    let key_cells = child_address_cells + child_interrupt_cells;
    if child_address_cells != 3 || child_interrupt_cells == 0 {
        return None;
    }

    let mut key = [0u32; 4];
    key[0] = ((bus as u32) << 16) | ((device as u32) << 11) | ((function as u32) << 8);
    key[1] = 0;
    key[2] = 0;
    key[3] = pin as u32;

    let mask_bytes = node.property("interrupt-map-mask")?.value;
    if mask_bytes.len() < key_cells * 4 {
        return None;
    }
    let mut mask = [0u32; 4];
    for (idx, word) in mask_bytes.chunks_exact(4).take(key_cells).enumerate() {
        mask[idx] = u32::from_be_bytes(word.try_into().ok()?);
    }

    let map = node.property("interrupt-map")?.value;
    let mut offset = 0usize;
    while offset + (key_cells + 1) * 4 <= map.len() {
        let mut child = [0u32; 4];
        for (idx, slot) in child.iter_mut().enumerate().take(key_cells) {
            let start = offset + idx * 4;
            *slot = u32::from_be_bytes(map[start..start + 4].try_into().ok()?);
        }
        offset += key_cells * 4;

        let phandle = u32::from_be_bytes(map[offset..offset + 4].try_into().ok()?);
        offset += 4;
        let controller_node = find_node_by_phandle(phandle)?;
        let parent_address_cells =
            property_u32(controller_node, "#address-cells").unwrap_or(0) as usize;
        let parent_interrupt_cells =
            property_u32(controller_node, "#interrupt-cells").unwrap_or(0) as usize;
        let parent_total_cells = parent_address_cells + parent_interrupt_cells;
        if offset + parent_total_cells * 4 > map.len() {
            return None;
        }

        let mut parent = [0u32; 4];
        if parent_total_cells > parent.len() {
            return None;
        }
        for (idx, slot) in parent.iter_mut().enumerate().take(parent_total_cells) {
            let start = offset + idx * 4;
            *slot = u32::from_be_bytes(map[start..start + 4].try_into().ok()?);
        }
        offset += parent_total_cells * 4;

        let matched = child
            .iter()
            .zip(mask.iter())
            .zip(key.iter())
            .take(key_cells)
            .all(|((&child, &mask), &key)| (child & mask) == (key & mask));
        if !matched {
            continue;
        }

        let controller = controller_kind(controller_node);
        return parse_interrupt_by_controller(
            controller,
            &parent[parent_address_cells..parent_total_cells],
        );
    }

    None
}

pub fn interrupt_controller() -> Option<InterruptController<'static, 'static>> {
    fdt()?.interrupt_controller()
}

pub fn dice_region() -> Option<crate::MemoryRegion> {
    fdt()?.dice()?.regions()?.next()
}

fn collect_regions<const N: usize>(
    source: impl Iterator<Item = crate::MemoryRegion>,
) -> ([crate::MemoryRegion; N], usize) {
    let mut regions = [crate::MemoryRegion {
        starting_address: core::ptr::null(),
        size: 0,
    }; N];
    let mut count = 0;

    for region in source {
        if region.size == 0 {
            continue;
        }
        if count == N {
            return (regions, count);
        }
        regions[count] = region;
        count += 1;
    }

    (regions, count)
}

fn collect_reserved_regions<const N: usize>(
    source: impl Iterator<Item = crate::MemoryRegion>,
) -> ([crate::MemoryRegion; N], usize) {
    let (mut regions, mut count) = collect_regions(source);

    if count != 0 {
        regions[..count].sort_unstable_by_key(|region| region.starting_address as usize);

        let mut write = 0;
        for read in 1..count {
            let cur_start = regions[write].starting_address as usize;
            let cur_end = cur_start + regions[write].size;
            let next_start = regions[read].starting_address as usize;
            let next_end = next_start + regions[read].size;

            if next_start <= cur_end {
                regions[write].size = cur_end.max(next_end) - cur_start;
            } else {
                write += 1;
                regions[write] = regions[read];
            }
        }
        count = write + 1;
    }

    (regions, count)
}

fn collect_named_regions<const N: usize>(
    source: impl Iterator<Item = NamedMemoryRegion>,
) -> ([NamedMemoryRegion; N], usize) {
    let mut regions = [NamedMemoryRegion {
        region: crate::MemoryRegion {
            starting_address: core::ptr::null(),
            size: 0,
        },
        name: "",
    }; N];
    let mut count = 0;

    for region in source {
        if region.region.size == 0 {
            continue;
        }
        if count == N {
            return (regions, count);
        }
        regions[count] = region;
        count += 1;
    }

    (regions, count)
}

/// Read `/memory` regions directly from a DTB pointer without touching the
/// global DTB state.
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn read_memory_regions_from_ptr<const N: usize>(
    ptr: *const u8,
) -> Result<([crate::MemoryRegion; N], usize), FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(collect_regions(fdt.memory_regions()))
}

/// Read reserved-memory and memreserve entries directly from a DTB pointer
/// without touching the global DTB state.
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn read_reserved_memory_regions_from_ptr<const N: usize>(
    ptr: *const u8,
) -> Result<([crate::MemoryRegion; N], usize), FirmwareInitError> {
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(collect_reserved_regions(
        fdt.mem_reservations().chain(fdt.reserved_memory_regions()),
    ))
}

pub fn read_memory_regions<const N: usize>() -> ([crate::MemoryRegion; N], usize) {
    fdt()
        .map(|fdt| collect_regions(fdt.memory_regions()))
        .unwrap_or((
            [crate::MemoryRegion {
                starting_address: core::ptr::null(),
                size: 0,
            }; N],
            0,
        ))
}

pub fn read_reserved_memory_regions<const N: usize>() -> ([crate::MemoryRegion; N], usize) {
    fdt()
        .map(|fdt| {
            collect_reserved_regions(fdt.mem_reservations().chain(fdt.reserved_memory_regions()))
        })
        .unwrap_or((
            [crate::MemoryRegion {
                starting_address: core::ptr::null(),
                size: 0,
            }; N],
            0,
        ))
}

pub fn read_named_reserved_memory_regions<const N: usize>() -> ([NamedMemoryRegion; N], usize) {
    fdt()
        .map(|fdt| {
            let memreserve = fdt.mem_reservations().map(|region| NamedMemoryRegion {
                region,
                name: "dtb memreserve",
            });
            let reserved_nodes = fdt.reserved_memory_nodes().flat_map(|node| {
                let name = node.compatible().unwrap_or(node.name);
                node.reg()
                    .into_iter()
                    .flatten()
                    .map(move |region| NamedMemoryRegion { region, name })
            });
            collect_named_regions(memreserve.chain(reserved_nodes))
        })
        .unwrap_or((
            [NamedMemoryRegion {
                region: crate::MemoryRegion {
                    starting_address: core::ptr::null(),
                    size: 0,
                },
                name: "",
            }; N],
            0,
        ))
}
