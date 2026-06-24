// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;

use alloc::{boxed::Box, vec, vec::Vec};

use unittest::{assert_eq, def_test};

use super::*;

const FDT_MAGIC: u32 = 0xd00dfeed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_END: u32 = 5;

struct TestDtbBuilder {
    structs: Vec<u8>,
    strings: Vec<u8>,
    memreserve: Vec<(u64, u64)>,
}

impl TestDtbBuilder {
    fn new() -> Self {
        Self {
            structs: Vec::new(),
            strings: Vec::new(),
            memreserve: Vec::new(),
        }
    }

    fn push_be32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_be64(buf: &mut Vec<u8>, value: u64) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn align(buf: &mut Vec<u8>) {
        while !buf.len().is_multiple_of(4) {
            buf.push(0);
        }
    }

    fn begin_node(&mut self, name: &str) {
        Self::push_be32(&mut self.structs, FDT_BEGIN_NODE);
        self.structs.extend_from_slice(name.as_bytes());
        self.structs.push(0);
        Self::align(&mut self.structs);
    }

    fn end_node(&mut self) {
        Self::push_be32(&mut self.structs, FDT_END_NODE);
    }

    fn string_offset(&mut self, name: &str) -> u32 {
        let target = name.as_bytes();
        let mut offset = 0usize;
        while offset < self.strings.len() {
            let tail = &self.strings[offset..];
            let Some(end) = tail.iter().position(|&byte| byte == 0) else {
                break;
            };
            if &tail[..end] == target {
                return offset as u32;
            }
            offset += end + 1;
        }

        let offset = self.strings.len() as u32;
        self.strings.extend_from_slice(target);
        self.strings.push(0);
        offset
    }

    fn prop(&mut self, name: &str, value: &[u8]) {
        let name_off = self.string_offset(name);
        Self::push_be32(&mut self.structs, FDT_PROP);
        Self::push_be32(&mut self.structs, value.len() as u32);
        Self::push_be32(&mut self.structs, name_off);
        self.structs.extend_from_slice(value);
        Self::align(&mut self.structs);
    }

    fn prop_empty(&mut self, name: &str) {
        self.prop(name, &[]);
    }

    fn prop_str(&mut self, name: &str, value: &str) {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        self.prop(name, &bytes);
    }

    fn prop_u32(&mut self, name: &str, value: u32) {
        self.prop(name, &value.to_be_bytes());
    }

    fn prop_u32s(&mut self, name: &str, values: &[u32]) {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        self.prop(name, &bytes);
    }

    fn prop_reg64(&mut self, name: &str, values: &[(u64, u64)]) {
        let mut bytes = Vec::with_capacity(values.len() * 16);
        for &(addr, size) in values {
            bytes.extend_from_slice(&((addr >> 32) as u32).to_be_bytes());
            bytes.extend_from_slice(&(addr as u32).to_be_bytes());
            bytes.extend_from_slice(&((size >> 32) as u32).to_be_bytes());
            bytes.extend_from_slice(&(size as u32).to_be_bytes());
        }
        self.prop(name, &bytes);
    }

    fn add_memreserve(&mut self, address: u64, size: u64) {
        self.memreserve.push((address, size));
    }

    fn finish(mut self) -> Vec<u8> {
        Self::push_be32(&mut self.structs, FDT_END);

        let header_size = 10 * 4;
        let mem_rsvmap_size = (self.memreserve.len() + 1) * 16;
        let off_mem_rsvmap = header_size as u32;
        let off_dt_struct = (header_size + mem_rsvmap_size) as u32;
        let off_dt_strings = off_dt_struct + self.structs.len() as u32;
        let totalsize = off_dt_strings + self.strings.len() as u32;

        let mut dtb = Vec::new();
        Self::push_be32(&mut dtb, FDT_MAGIC);
        Self::push_be32(&mut dtb, totalsize);
        Self::push_be32(&mut dtb, off_dt_struct);
        Self::push_be32(&mut dtb, off_dt_strings);
        Self::push_be32(&mut dtb, off_mem_rsvmap);
        Self::push_be32(&mut dtb, 17);
        Self::push_be32(&mut dtb, 16);
        Self::push_be32(&mut dtb, 0);
        Self::push_be32(&mut dtb, self.strings.len() as u32);
        Self::push_be32(&mut dtb, self.structs.len() as u32);
        for (address, size) in self.memreserve {
            Self::push_be64(&mut dtb, address);
            Self::push_be64(&mut dtb, size);
        }
        Self::push_be64(&mut dtb, 0);
        Self::push_be64(&mut dtb, 0);
        dtb.extend_from_slice(&self.structs);
        dtb.extend_from_slice(&self.strings);
        dtb
    }
}

fn build_test_dtb() -> Vec<u8> {
    let mut builder = TestDtbBuilder::new();
    builder.add_memreserve(0x8100_0000, 0x2000);

    builder.begin_node("");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.prop_str("model", "test-board");

    builder.begin_node("cpus");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 0);

    builder.begin_node("cpu@0");
    builder.prop_str("device_type", "cpu");
    builder.prop_u32s("reg", &[0, 0]);
    builder.end_node();

    builder.begin_node("cpu@1");
    builder.prop_str("device_type", "cpu");
    builder.prop_u32s("reg", &[0, 1]);
    builder.prop_str("status", "disabled");
    builder.end_node();
    builder.end_node();

    builder.begin_node("memory@80000000");
    builder.prop_str("device_type", "memory");
    builder.prop_reg64("reg", &[(0x8000_0000, 0x4000_0000)]);
    builder.end_node();

    builder.begin_node("intc");
    builder.prop_u32("phandle", 1);
    builder.prop_str("compatible", "arm,gic-400");
    builder.prop_empty("interrupt-controller");
    builder.prop_u32("#interrupt-cells", 3);
    builder.end_node();

    builder.begin_node("plic@c000000");
    builder.prop_u32("phandle", 2);
    builder.prop_str("compatible", "sifive,plic-1.0.0");
    builder.prop_empty("interrupt-controller");
    builder.prop_u32("#interrupt-cells", 1);
    builder.end_node();

    builder.begin_node("uart@1000");
    builder.prop_u32("interrupt-parent", 1);
    builder.prop_u32s("interrupts", &[0, 5, 4]);
    builder.end_node();

    builder.begin_node("virtio@2000");
    builder.prop_u32s("interrupts-extended", &[2, 9]);
    builder.end_node();

    builder.begin_node("pci@30000000");
    builder.prop_str("compatible", "pci-host-ecam-generic");
    builder.prop_u32("#address-cells", 3);
    builder.prop_u32("#size-cells", 2);
    builder.prop_u32("#interrupt-cells", 1);
    builder.prop_reg64("reg", &[(0x3000_0000, 0x0100_0000)]);
    builder.prop_u32s("bus-range", &[0, 0xff]);
    builder.prop_u32s(
        "ranges",
        &[
            0x4200_0000,
            0,
            0,
            0,
            0x8000_0000,
            0,
            0x1000_0000,
            0x0200_0000,
            0,
            0,
            0,
            0x9000_0000,
            0,
            0x0800_0000,
        ],
    );
    builder.prop_u32s("interrupt-map-mask", &[0x0000_f800, 0, 0, 0x7]);
    builder.prop_u32s("interrupt-map", &[0x0000_0800, 0, 0, 1, 1, 0, 9, 4]);
    builder.end_node();

    builder.begin_node("reserved-memory");
    builder.prop_u32("#address-cells", 2);
    builder.prop_u32("#size-cells", 2);
    builder.prop("ranges", &[]);

    builder.begin_node("region@81001000");
    builder.prop_str("compatible", "shared-dma-pool");
    builder.prop_reg64("reg", &[(0x8100_1000, 0x3000), (0x8200_0000, 0x1000)]);
    builder.end_node();

    builder.begin_node("empty@0");
    builder.prop_reg64("reg", &[(0x8300_0000, 0)]);
    builder.end_node();
    builder.end_node();

    builder.end_node();
    builder.finish()
}

fn test_dtb_bytes() -> &'static [u8] {
    Box::leak(build_test_dtb().into_boxed_slice())
}

fn test_fdt() -> &'static LinuxFdt<'static> {
    // SAFETY: The leaked DTB bytes live for the rest of the test process
    // and form a valid flattened device tree blob.
    let fdt = unsafe { LinuxFdt::from_ptr(test_dtb_bytes().as_ptr()) }.unwrap();
    Box::leak(Box::new(fdt))
}

fn test_find_node_by_phandle(
    fdt: &'static LinuxFdt<'static>,
    phandle: u32,
) -> Option<FdtNode<'static, 'static>> {
    fdt.all_nodes().find(move |node| {
        node.property_u32("phandle")
            .or_else(|| node.property_u32("linux,phandle"))
            == Some(phandle)
    })
}

fn test_interrupt_parent_node(
    fdt: &'static LinuxFdt<'static>,
    node: FdtNode<'static, 'static>,
) -> Option<FdtNode<'static, 'static>> {
    let phandle = node.property_u32("interrupt-parent").or_else(|| {
        node.parent_property("interrupt-parent")
            .and_then(|prop| prop.value.get(..4))
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_be_bytes)
    })?;
    test_find_node_by_phandle(fdt, phandle)
}

fn test_controller_kind(node: FdtNode<'static, 'static>) -> InterruptControllerKind {
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

fn test_first_interrupt_desc(
    fdt: &'static LinuxFdt<'static>,
    node: FdtNode<'static, 'static>,
) -> Option<InterruptInfo> {
    if let Some(parent) = test_interrupt_parent_node(fdt, node) {
        let controller = test_controller_kind(parent);
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
        let controller_node = test_find_node_by_phandle(fdt, cells[0])?;
        let controller = test_controller_kind(controller_node);
        let irq = parse_interrupt_by_controller(controller, &cells[1..])?;
        return Some(irq);
    }

    if let Some(cells) = property_u32_cells::<2>(node, "interrupts-extended") {
        let controller_node = test_find_node_by_phandle(fdt, cells[0])?;
        let controller = test_controller_kind(controller_node);
        let irq = parse_interrupt_by_controller(controller, &cells[1..])?;
        return Some(irq);
    }

    None
}

fn test_generic_pci_host(fdt: &'static LinuxFdt<'static>) -> Option<FdtNode<'static, 'static>> {
    fdt.find_compatible("pci-host-ecam-generic")
        .or_else(move || fdt.find_compatible("pci-host-cam-generic"))
}

fn test_generic_pci_host_info(fdt: &'static LinuxFdt<'static>) -> Option<PciHostInfo> {
    let node = test_generic_pci_host(fdt)?;
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

fn test_generic_pci_non_prefetchable_mem_range(
    fdt: &'static LinuxFdt<'static>,
) -> Option<PciRangeInfo> {
    const PCI_ADDR_SPACE_MASK: u32 = 0x0300_0000;
    const PCI_ADDR_SPACE_MEM32: u32 = 0x0200_0000;
    const PCI_ADDR_SPACE_MEM64: u32 = 0x0300_0000;
    const PCI_ADDR_PREFETCH: u32 = 0x4000_0000;

    let node = test_generic_pci_host(fdt)?;
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

fn test_generic_pci_legacy_interrupt(
    fdt: &'static LinuxFdt<'static>,
    bus: u8,
    device: u8,
    function: u8,
    pin: u8,
) -> Option<InterruptInfo> {
    let node = test_generic_pci_host(fdt)?;
    let child_address_cells = node.cell_sizes().address_cells;
    let child_interrupt_cells = node.property_u32("#interrupt-cells").unwrap_or(1) as usize;
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
        let controller_node = test_find_node_by_phandle(fdt, phandle)?;
        let parent_address_cells =
            controller_node.property_u32("#address-cells").unwrap_or(0) as usize;
        let parent_interrupt_cells = controller_node
            .property_u32("#interrupt-cells")
            .unwrap_or(0) as usize;
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

        let controller = test_controller_kind(controller_node);
        return parse_interrupt_by_controller(
            controller,
            &parent[parent_address_cells..parent_total_cells],
        );
    }

    None
}

fn test_read_named_reserved_memory_regions<const N: usize>(
    fdt: &'static LinuxFdt<'static>,
) -> ([NamedMemoryRegion; N], usize) {
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
}

#[def_test]
fn cpu_helpers_skip_disabled_nodes_and_parse_two_cell_reg() {
    let fdt = test_fdt();
    let cpus = enabled_cpu_nodes(fdt).collect::<Vec<_>>();
    assert_eq!(cpus.len(), 1);
    assert_eq!(cpus[0].name, "cpu@0");
    assert_eq!(cpu_node_reg(cpus[0]), Some(0));

    let memory = fdt.find_node("/memory@80000000").unwrap();
    assert_eq!(cpu_node_reg(memory), None);
}

#[def_test]
fn first_interrupt_desc_decodes_interrupt_parent_gic_irq() {
    let fdt = test_fdt();
    let uart = fdt.find_node("/uart@1000").unwrap();
    assert_eq!(
        test_first_interrupt_desc(fdt, uart),
        Some(InterruptInfo {
            irq: 37,
            trigger: InterruptTrigger::LevelHigh,
            controller: InterruptControllerKind::Gic,
        })
    );
}

#[def_test]
fn first_interrupt_desc_decodes_interrupts_extended_plic_irq() {
    let fdt = test_fdt();
    let virtio = fdt.find_node("/virtio@2000").unwrap();
    assert_eq!(
        test_first_interrupt_desc(fdt, virtio),
        Some(InterruptInfo {
            irq: 9,
            trigger: InterruptTrigger::Unknown(0),
            controller: InterruptControllerKind::Plic,
        })
    );
}

#[def_test]
fn pci_host_helpers_cover_ecam_ranges_and_legacy_irq_map() {
    let fdt = test_fdt();
    assert_eq!(
        test_generic_pci_host_info(fdt),
        Some(PciHostInfo {
            cam: PciHostCam::Ecam,
            ecam_base: 0x3000_0000,
            ecam_size: 0x0100_0000,
            bus_start: 0,
            bus_end: 0xff,
        })
    );

    assert_eq!(
        test_generic_pci_non_prefetchable_mem_range(fdt),
        Some(PciRangeInfo {
            cpu_base: 0x9000_0000,
            size: 0x0800_0000,
            prefetchable: false,
        })
    );

    assert_eq!(
        test_generic_pci_legacy_interrupt(fdt, 0, 1, 0, 1),
        Some(InterruptInfo {
            irq: 41,
            trigger: InterruptTrigger::LevelHigh,
            controller: InterruptControllerKind::Gic,
        })
    );
}

#[def_test]
fn reserved_memory_helpers_merge_overlaps_and_keep_names() {
    let fdt = test_fdt();
    let (regions, count) =
        // SAFETY: `test_dtb_bytes()` returns a valid, fully-initialized DTB
        // byte buffer; its pointer is aligned, non-null, and valid for the
        // buffer length, and the callee reads only within those bounds.
        unsafe { read_reserved_memory_regions_from_ptr::<4>(test_dtb_bytes().as_ptr()) }.unwrap();
    assert_eq!(count, 2);
    assert_eq!(regions[0].starting_address as usize, 0x8100_0000);
    assert_eq!(regions[0].size, 0x4000);
    assert_eq!(regions[1].starting_address as usize, 0x8200_0000);
    assert_eq!(regions[1].size, 0x1000);

    let (named, named_count) = test_read_named_reserved_memory_regions::<4>(fdt);
    assert_eq!(named_count, 3);
    assert_eq!(named[0].name, "dtb memreserve");
    assert_eq!(named[0].region.starting_address as usize, 0x8100_0000);
    assert_eq!(named[1].name, "shared-dma-pool");
    assert_eq!(named[1].region.starting_address as usize, 0x8100_1000);
    assert_eq!(named[2].name, "shared-dma-pool");
    assert_eq!(named[2].region.starting_address as usize, 0x8200_0000);
}

#[def_test]
fn parse_gic_interrupt_accepts_raw_irq_fallback() {
    assert_eq!(
        parse_gic_interrupt(&[7]),
        Some(InterruptInfo {
            irq: 7,
            trigger: InterruptTrigger::Unknown(0),
            controller: InterruptControllerKind::Gic,
        })
    );
}

#[def_test]
fn collect_reserved_regions_merges_sorted_overlaps() {
    let source = vec![
        crate::MemoryRegion {
            starting_address: 0x3000 as *const u8,
            size: 0x100,
        },
        crate::MemoryRegion {
            starting_address: 0x1000 as *const u8,
            size: 0x200,
        },
        crate::MemoryRegion {
            starting_address: 0x1100 as *const u8,
            size: 0x200,
        },
        crate::MemoryRegion {
            starting_address: 0x5000 as *const u8,
            size: 0,
        },
    ];
    let (regions, count) = collect_reserved_regions::<4>(source.into_iter());

    assert_eq!(count, 2);
    assert_eq!(regions[0].starting_address as usize, 0x1000);
    assert_eq!(regions[0].size, 0x300);
    assert_eq!(regions[1].starting_address as usize, 0x3000);
    assert_eq!(regions[1].size, 0x100);
}

#[def_test]
fn read_memory_regions_from_ptr_reports_memory_node_ranges() {
    let (regions, count) =
        // SAFETY: `test_dtb_bytes()` returns a valid, fully-initialized DTB
        // byte buffer; its pointer is aligned, non-null, and valid for the
        // buffer length, and the callee reads only within those bounds.
        unsafe { read_memory_regions_from_ptr::<2>(test_dtb_bytes().as_ptr()) }.unwrap();
    assert_eq!(count, 1);
    assert_eq!(regions[0].starting_address as usize, 0x8000_0000);
    assert_eq!(regions[0].size, 0x4000_0000);
}
