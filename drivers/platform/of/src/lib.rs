// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-tree access helpers.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};

use lazyinit::LazyInit;
use memaddr::PhysAddr;
pub use rs_fdtree::{
    Chosen, Dice, FdtError, FdtNode, InterruptController, LinuxFdt, MemoryRegion, NodeProperty,
    RegIter,
};

/// Failure to initialize the global device-tree handle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FirmwareInitError {
    /// The boot pointer was null.
    MissingDeviceTreePtr,
    /// The blob did not validate as a flattened device tree.
    BadDeviceTree(FdtError),
}

static FDT: LazyInit<LinuxFdt<'static>> = LazyInit::new();

/// Interrupt trigger mode decoded from firmware cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptTrigger {
    /// Interrupt asserted on the rising edge.
    EdgeRising,
    /// Interrupt asserted on the falling edge.
    EdgeFalling,
    /// Interrupt active while the line is high.
    LevelHigh,
    /// Interrupt active while the line is low.
    LevelLow,
    /// Undecoded trigger flags; carries the raw flag bits.
    Unknown(u32),
}

/// Interrupt controller family owning a decoded interrupt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptControllerKind {
    /// ARM GIC (v2/v3/v4) family.
    Gic,
    /// RISC-V PLIC.
    Plic,
    /// Controller whose compatible string this crate does not know.
    Unknown,
}

/// Configuration access mechanism of a generic PCI host bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciHostCam {
    /// Legacy memory-mapped config access (256 bytes per device function).
    Cam,
    /// PCI Express enhanced configuration access mechanism (4 KiB per device).
    Ecam,
}

/// Facts about a generic PCI host bridge: config window, mechanism, and bus
/// range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciHostInfo {
    /// Config access mechanism (`Cam` or `Ecam`).
    pub cam: PciHostCam,
    /// Physical base of the config space window.
    pub ecam_base: u64,
    /// Size of the config space window in bytes.
    pub ecam_size: u64,
    /// First bus number in the bridge's `bus-range`.
    pub bus_start: u8,
    /// Last bus number in the bridge's `bus-range`.
    pub bus_end: u8,
}

/// One CPU-visible memory window from a PCI host bridge's `ranges`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciRangeInfo {
    /// CPU physical base address of the window.
    pub cpu_base: u64,
    /// Window size in bytes.
    pub size: u64,
    /// Whether the range is marked prefetchable.
    pub prefetchable: bool,
}

/// A fully decoded interrupt: number, trigger mode, and owning controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptInfo {
    /// Global IRQ number (GIC SPI/PPI numbering applied).
    pub irq: usize,
    /// Decoded trigger mode.
    pub trigger: InterruptTrigger,
    /// Controller family the interrupt belongs to.
    pub controller: InterruptControllerKind,
}

/// A reserved-memory region together with its DT node name (or
/// `memreserve` provenance), for debugging and policy decisions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NamedMemoryRegion {
    /// The reserved region itself.
    pub region: crate::MemoryRegion,
    /// Node name or `dtb memreserve` tag the region came from.
    pub name: &'static str,
}

fn property_u32_cells<const N: usize>(node: FdtNode<'_, '_>, name: &str) -> Option<[u32; N]> {
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

/// Reads a named property as a big-endian `u32`.
pub fn property_u32(node: FdtNode<'static, 'static>, name: &str) -> Option<u32> {
    node.property_u32(name)
}

/// Returns whether the node declares `device_type = "cpu"`.
pub fn is_cpu_node(node: FdtNode<'_, '_>) -> bool {
    node.property_str("device_type") == Some("cpu")
}

/// Returns whether the node is a CPU node whose `status` is not `disabled`.
pub fn is_enabled_cpu_node(node: FdtNode<'_, '_>) -> bool {
    is_cpu_node(node) && node.property_str("status") != Some("disabled")
}

/// Returns the first `reg` address of a CPU node, honoring the parent's
/// `#address-cells` width.
pub fn cpu_node_reg(node: FdtNode<'_, '_>) -> Option<u64> {
    if !is_cpu_node(node) {
        return None;
    }
    node_reg(node)
}

/// The first `reg` address of a node, honoring the parent's
/// `#address-cells` width.
fn node_reg(node: FdtNode<'_, '_>) -> Option<u64> {
    let address_cells = node.parent_property_u32("#address-cells").unwrap_or(1) as usize;
    match address_cells {
        1 => property_u32_cells::<1>(node, "reg").and_then(|cells| parse_cells_u64(&cells)),
        2 => property_u32_cells::<2>(node, "reg").and_then(|cells| parse_cells_u64(&cells)),
        _ => None,
    }
}

/// Iterates all CPU nodes that are not `status = "disabled"`.
pub fn enabled_cpu_nodes<'dt>(
    fdt: &'dt LinuxFdt<'dt>,
) -> impl Iterator<Item = FdtNode<'dt, 'dt>> + 'dt {
    fdt.all_nodes().filter(|node| is_enabled_cpu_node(*node))
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
    find_node_by_phandle_in(fdt()?, phandle)
}

fn find_node_by_phandle_in(
    fdt: &'static LinuxFdt<'static>,
    phandle: u32,
) -> Option<FdtNode<'static, 'static>> {
    fdt.all_nodes().find(|node| {
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

/// Returns the global device-tree view initialized at boot; `None` before
/// `init_device_tree_ptr` has run.
pub fn fdt() -> Option<&'static LinuxFdt<'static>> {
    FDT.get()
}

/// Read the total size of the DTB referenced by `ptr`.
///
/// # Errors
///
/// Returns [`FirmwareInitError::BadDeviceTree`] when the blob fails
/// `rs_fdtree` validation (a null pointer is reported the same way).
///
/// # Safety
///
/// `ptr` must point to a valid, readable DTB blob for the duration of this
/// call.
pub unsafe fn dtb_total_size_from_ptr(ptr: *const u8) -> Result<usize, FirmwareInitError> {
    // SAFETY: The caller guarantees `ptr` points to a valid, readable DTB blob
    // for the duration of this call.
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(fdt.total_size())
}

/// Returns the total size in bytes of the initialized DTB.
pub fn dtb_total_size() -> Option<usize> {
    Some(fdt()?.total_size())
}

/// Initialize the global DTB handle from a raw pointer.
///
/// # Errors
///
/// Returns [`FirmwareInitError::BadDeviceTree`] when the blob fails
/// `rs_fdtree` validation (a null pointer is reported the same way).
/// The [`FirmwareInitError::MissingDeviceTreePtr`] variant is not
/// produced by this function.
///
/// # Safety
///
/// `ptr` must point to a valid DTB blob that remains accessible for the rest of
/// the program lifetime.
pub unsafe fn init_device_tree_ptr(ptr: *const u8) -> Result<(), FirmwareInitError> {
    // SAFETY: The caller guarantees `ptr` points to a valid, readable DTB blob
    // that remains accessible for the rest of the program lifetime.
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    FDT.init_once(fdt);
    Ok(())
}

/// Returns the kernel command line from the `/chosen` node.
pub fn chosen_bootargs() -> Option<&'static str> {
    fdt()?.chosen_bootargs()
}

/// Returns the root node's `model` string.
pub fn root_model() -> Option<&'static str> {
    fdt()?.root_model()
}

/// Returns the root node's first `compatible` string.
pub fn root_compatible() -> Option<&'static str> {
    fdt()?.root_compatible()
}

/// Finds the first node whose `compatible` list contains the string.
pub fn find_compatible(compatible: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.find_compatible(compatible)
}

/// Finds the generic PCI host bridge node (`pci-host-ecam-generic` or
/// `pci-host-cam-generic`).
pub fn generic_pci_host() -> Option<FdtNode<'static, 'static>> {
    generic_pci_host_in(fdt()?)
}

fn generic_pci_host_in(fdt: &'static LinuxFdt<'static>) -> Option<FdtNode<'static, 'static>> {
    fdt.find_compatible("pci-host-ecam-generic")
        .or_else(|| fdt.find_compatible("pci-host-cam-generic"))
}

/// Finds a node by absolute device-tree path.
pub fn find_node(path: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.find_node(path)
}

/// Returns the console device path from the `/chosen` node.
pub fn chosen_stdout_path() -> Option<&'static str> {
    fdt()?.chosen_stdout_path()
}

/// Returns the `/chosen` node wrapper.
pub fn chosen() -> Option<Chosen<'static, 'static>> {
    fdt()?.chosen()
}

/// Resolves a path or an `/aliases` alias to a node.
pub fn resolve_node(path_or_alias: &str) -> Option<FdtNode<'static, 'static>> {
    fdt()?.resolve_node(path_or_alias)
}

/// A control register declared in the DT `syscon-poweroff` style: the
/// physical address of the register and the vendor-encoded value to write
/// to it.
pub struct SysconControl {
    /// Physical address of the register — the `regmap` target's `reg` base
    /// plus the node's `offset` property.
    pub paddr: PhysAddr,
    /// The node's `value` property.
    pub value: u32,
}

/// The device tree's `syscon-poweroff` declaration, if present.
///
/// This is how a machine names its power-off register without ACPI tables:
/// the node points at a `syscon` register block through a `regmap` phandle
/// and declares the `offset` and `value` of the byte that removes power.
/// QEMU's loongarch virt machine describes its ACPI GED sleep-control
/// register this way — address and the whole S5 byte included — so direct
/// kernel boot (which hands over no ACPI table) still has a firmware
/// declaration to read instead of a guest-side constant.
pub fn syscon_poweroff() -> Option<SysconControl> {
    let node = find_compatible("syscon-poweroff")?;
    let regmap = find_node_by_phandle(property_u32(node, "regmap")?)?;
    let offset = u64::from(property_u32(node, "offset")?);
    Some(SysconControl {
        paddr: PhysAddr::from_usize((node_reg(regmap)? + offset) as usize),
        value: property_u32(node, "value")?,
    })
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

/// Cached PMU interrupt IRQ. `0` means "not yet resolved"; a real PMU IRQ is
/// never 0 on a GIC (SGIs start at 0 but the PMU uses an SPI/PPI >= 16).
static PMU_IRQ_CACHE: AtomicUsize = AtomicUsize::new(0);

fn is_arm_cpu_pmu_compatible(compatible: &str) -> bool {
    compatible == "arm,armv8-pmuv3"
        || (compatible.starts_with("arm,cortex-") && compatible.ends_with("-pmu"))
        || (compatible.starts_with("arm,neoverse-") && compatible.ends_with("-pmu"))
        || (compatible.starts_with("arm,c1-") && compatible.ends_with("-pmu"))
}

/// Resolve the PMU interrupt IRQ from an ARM CPU PMU device-tree node.
///
/// Linux's ARM PMU drivers match explicit CPU PMU compatibles such as
/// `arm,armv8-pmuv3`, `arm,cortex-a55-pmu`, and `arm,cortex-a76-pmu`; do the
/// same here instead of accepting any node whose compatible merely contains
/// `pmu`, which can also match SoC power-management blocks.
fn resolve_pmu_irq() -> Option<usize> {
    let fdt = fdt()?;
    fdt.all_nodes()
        .filter(|node| node.compatibles().any(is_arm_cpu_pmu_compatible))
        .find_map(|node| first_interrupt_desc(node).map(|info| info.irq))
}

/// Return the PMU interrupt IRQ number, resolved once from the device tree and
/// cached for subsequent (hot-path) calls. Falls back to `fallback` (e.g. the
/// Kconfig default) when the device tree has no PMU node.
pub fn pmu_irq_or(fallback: usize) -> usize {
    let cached = PMU_IRQ_CACHE.load(Ordering::Acquire);
    if cached != 0 {
        return cached;
    }
    let resolved = resolve_pmu_irq().unwrap_or(fallback);
    if resolved != 0 {
        PMU_IRQ_CACHE.store(resolved, Ordering::Release);
    }
    resolved
}

/// Describes the generic PCI host bridge: mechanism, config window, and
/// bus range.
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

/// Returns the host bridge's memory window from `ranges`, preferring the
/// non-prefetchable one and falling back to a prefetchable window.
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

/// Maximum parent specifier size in 32-bit cells, matching Linux's phandle cap.
const MAX_PHANDLE_ARGS: usize = 16;

/// Resolves a legacy INTx route through the bridge's `interrupt-map` for a
/// (bus, device, function, pin) address.
///
/// The PCI child specifier uses three address cells and one interrupt cell;
/// other child layouts are rejected with `None` before fixed-buffer decoding.
/// Parent address and interrupt cells share a bounded buffer. A parent
/// specifier exceeding that capacity is logged and rejected with `None`.
pub fn generic_pci_legacy_interrupt(
    bus: u8,
    device: u8,
    function: u8,
    pin: u8,
) -> Option<InterruptInfo> {
    pci_legacy_interrupt_from_fdt(fdt()?, bus, device, function, pin)
}

fn pci_legacy_interrupt_from_fdt(
    fdt: &'static LinuxFdt<'static>,
    bus: u8,
    device: u8,
    function: u8,
    pin: u8,
) -> Option<InterruptInfo> {
    let node = generic_pci_host_in(fdt)?;
    let child_address_cells = node.cell_sizes().address_cells;
    let child_interrupt_cells = property_u32(node, "#interrupt-cells").unwrap_or(1) as usize;
    if child_address_cells != 3 || child_interrupt_cells != 1 {
        return None;
    }
    let key_cells = child_address_cells + child_interrupt_cells;

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
        let controller_node = find_node_by_phandle_in(fdt, phandle)?;
        let parent_address_cells =
            property_u32(controller_node, "#address-cells").unwrap_or(0) as usize;
        let parent_interrupt_cells =
            property_u32(controller_node, "#interrupt-cells").unwrap_or(0) as usize;
        let parent_total_cells = parent_address_cells + parent_interrupt_cells;
        // The GIC parent may need two address cells plus three interrupt cells.
        let mut parent = [0u32; MAX_PHANDLE_ARGS];
        if parent_total_cells > parent.len() {
            log::warn!(
                "of: interrupt-map parent specifier needs {parent_total_cells} cells (> \
                 MAX_PHANDLE_ARGS {MAX_PHANDLE_ARGS})"
            );
            return None;
        }
        if offset + parent_total_cells * 4 > map.len() {
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

/// Returns the first node carrying an `interrupt-controller` property.
pub fn interrupt_controller() -> Option<InterruptController<'static, 'static>> {
    fdt()?.interrupt_controller()
}

/// Returns the Open DICE reserved-memory region, if the tree describes one.
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
    // SAFETY: The caller guarantees `ptr` points to a valid, readable DTB blob
    // for the duration of this call.
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
    // SAFETY: The caller guarantees `ptr` points to a valid, readable DTB blob
    // for the duration of this call.
    let fdt = unsafe { LinuxFdt::from_ptr(ptr) }.map_err(FirmwareInitError::BadDeviceTree)?;
    Ok(collect_reserved_regions(
        fdt.mem_reservations().chain(fdt.reserved_memory_regions()),
    ))
}

/// Collects `/memory` regions into a fixed-capacity array; returns the
/// zeroed array and a count of 0 when the tree is unavailable.
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

/// Collects memreserve entries plus `/reserved-memory` children into a
/// fixed-capacity array; returns the zeroed array and a count of 0 when the
/// tree is unavailable.
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

/// Collects reserved regions with their node names / `memreserve` tags;
/// returns the zeroed array and a count of 0 when the tree is unavailable.
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

// Gate macro resolution as well as the test module for early-boot consumers.
#[cfg(unittest)]
unittest::path_mod_test!(unittest_tests, "../ut-tests/of_unittest.rs");
