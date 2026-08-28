// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

static DTB_DATA: &[u8] = include_bytes!("../dtb/test.dtb");
static CROSVM_DTB_DATA: &[u8] = include_bytes!("../dtb/crosvm.dtb");

use rs_fdtree::LinuxFdt;

fn setup() -> LinuxFdt<'static> {
    LinuxFdt::new(DTB_DATA).unwrap()
}

fn setup_crosvm() -> LinuxFdt<'static> {
    LinuxFdt::new(CROSVM_DTB_DATA).unwrap()
}

#[test]
fn parse_fdt() {
    let fdt = setup();
    assert_eq!(fdt.total_size(), DTB_DATA.len());
    assert_eq!(fdt.as_bytes(), DTB_DATA);
}

#[test]
fn fdt_bytes_exclude_trailing_buffer_data() {
    let mut data = DTB_DATA.to_vec();
    data.extend_from_slice(&[0xaa; 16]);

    let fdt = LinuxFdt::new(&data).unwrap();
    assert_eq!(fdt.as_bytes(), DTB_DATA);
}

#[test]
fn interrupt_controller() {
    let fdt = setup();
    let controller = fdt.interrupt_controller().unwrap();
    assert_eq!(controller.compatible(), Some("riscv,cpu-intc"));
}

fn build_dice_dtb() -> Vec<u8> {
    const FDT_MAGIC: u32 = 0xd00dfeed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 5;

    fn push_be32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_name(buf: &mut Vec<u8>, name: &[u8]) {
        buf.extend_from_slice(name);
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    fn push_prop(buf: &mut Vec<u8>, name_off: u32, value: &[u8]) {
        push_be32(buf, FDT_PROP);
        push_be32(buf, value.len() as u32);
        push_be32(buf, name_off);
        buf.extend_from_slice(value);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    let strings = b"#address-cells\0#size-cells\0compatible\0reg\0";
    let off_addr_cells = 0u32;
    let off_size_cells = 15u32;
    let off_compatible = 27u32;
    let off_reg = 38u32;

    let mut structs = Vec::new();
    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"reserved-memory");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"dice");
    let reg = [
        0u32.to_be_bytes(),
        0x1234_5000u32.to_be_bytes(),
        0u32.to_be_bytes(),
        0x1000u32.to_be_bytes(),
    ]
    .concat();
    push_prop(&mut structs, off_compatible, b"google,open-dice\0");
    push_prop(&mut structs, off_reg, &reg);
    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END);

    let header_size = 10 * 4;
    let mem_rsvmap_size = 16;
    let off_mem_rsvmap = header_size as u32;
    let off_dt_struct = (header_size + mem_rsvmap_size) as u32;
    let off_dt_strings = off_dt_struct + structs.len() as u32;
    let totalsize = off_dt_strings + strings.len() as u32;

    let mut dtb = Vec::new();
    push_be32(&mut dtb, FDT_MAGIC);
    push_be32(&mut dtb, totalsize);
    push_be32(&mut dtb, off_dt_struct);
    push_be32(&mut dtb, off_dt_strings);
    push_be32(&mut dtb, off_mem_rsvmap);
    push_be32(&mut dtb, 17);
    push_be32(&mut dtb, 16);
    push_be32(&mut dtb, 0);
    push_be32(&mut dtb, strings.len() as u32);
    push_be32(&mut dtb, structs.len() as u32);
    dtb.extend_from_slice(&[0; 16]);
    dtb.extend_from_slice(&structs);
    dtb.extend_from_slice(strings);
    dtb
}

fn build_reserved_memory_dtb() -> Vec<u8> {
    const FDT_MAGIC: u32 = 0xd00dfeed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 5;

    fn push_be32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_be64(buf: &mut Vec<u8>, value: u64) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_name(buf: &mut Vec<u8>, name: &[u8]) {
        buf.extend_from_slice(name);
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    fn push_prop(buf: &mut Vec<u8>, name_off: u32, value: &[u8]) {
        push_be32(buf, FDT_PROP);
        push_be32(buf, value.len() as u32);
        push_be32(buf, name_off);
        buf.extend_from_slice(value);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    let strings = b"#address-cells\0#size-cells\0reg\0";
    let off_addr_cells = 0u32;
    let off_size_cells = 15u32;
    let off_reg = 27u32;

    let mut structs = Vec::new();
    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"reserved-memory");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"region@81000000");
    let reg = [
        0u32.to_be_bytes(),
        0x8100_0000u32.to_be_bytes(),
        0u32.to_be_bytes(),
        0x2000u32.to_be_bytes(),
        0u32.to_be_bytes(),
        0x8200_0000u32.to_be_bytes(),
        0u32.to_be_bytes(),
        0x3000u32.to_be_bytes(),
    ]
    .concat();
    push_prop(&mut structs, off_reg, &reg);
    push_be32(&mut structs, FDT_END_NODE);

    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END);

    let header_size = 10 * 4;
    let mem_rsvmap_size = 32;
    let off_mem_rsvmap = header_size as u32;
    let off_dt_struct = (header_size + mem_rsvmap_size) as u32;
    let off_dt_strings = off_dt_struct + structs.len() as u32;
    let totalsize = off_dt_strings + strings.len() as u32;

    let mut dtb = Vec::new();
    push_be32(&mut dtb, FDT_MAGIC);
    push_be32(&mut dtb, totalsize);
    push_be32(&mut dtb, off_dt_struct);
    push_be32(&mut dtb, off_dt_strings);
    push_be32(&mut dtb, off_mem_rsvmap);
    push_be32(&mut dtb, 17);
    push_be32(&mut dtb, 16);
    push_be32(&mut dtb, 0);
    push_be32(&mut dtb, strings.len() as u32);
    push_be32(&mut dtb, structs.len() as u32);
    push_be64(&mut dtb, 0x8000_0000);
    push_be64(&mut dtb, 0x1000);
    push_be64(&mut dtb, 0);
    push_be64(&mut dtb, 0);
    dtb.extend_from_slice(&structs);
    dtb.extend_from_slice(strings);
    dtb
}

fn build_chosen_memory_dtb() -> Vec<u8> {
    const FDT_MAGIC: u32 = 0xd00dfeed;
    const FDT_BEGIN_NODE: u32 = 1;
    const FDT_END_NODE: u32 = 2;
    const FDT_PROP: u32 = 3;
    const FDT_END: u32 = 5;

    fn push_be32(buf: &mut Vec<u8>, value: u32) {
        buf.extend_from_slice(&value.to_be_bytes());
    }

    fn push_name(buf: &mut Vec<u8>, name: &[u8]) {
        buf.extend_from_slice(name);
        buf.push(0);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    fn push_prop(buf: &mut Vec<u8>, name_off: u32, value: &[u8]) {
        push_be32(buf, FDT_PROP);
        push_be32(buf, value.len() as u32);
        push_be32(buf, name_off);
        buf.extend_from_slice(value);
        while buf.len() % 4 != 0 {
            buf.push(0);
        }
    }

    let strings =
        b"#address-cells\0#size-cells\0bootargs\0stdout-path\0device_type\0reg\0serial0\0";
    let off_addr_cells = 0u32;
    let off_size_cells = 15u32;
    let off_bootargs = 27u32;
    let off_stdout_path = 36u32;
    let off_device_type = 48u32;
    let off_reg = 60u32;
    let off_serial0 = 64u32;

    let mut structs = Vec::new();
    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"chosen");
    push_prop(&mut structs, off_bootargs, b"console=ttyS0\0");
    push_prop(&mut structs, off_stdout_path, b"serial0:115200n8\0");
    push_be32(&mut structs, FDT_END_NODE);

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"aliases");
    push_prop(&mut structs, off_serial0, b"/soc/uart@10000000\0");
    push_be32(&mut structs, FDT_END_NODE);

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"memory@80000000");
    push_prop(&mut structs, off_device_type, b"memory\0");
    let reg = [
        0u32.to_be_bytes(),
        0x8000_0000u32.to_be_bytes(),
        0u32.to_be_bytes(),
        0x4000_0000u32.to_be_bytes(),
    ]
    .concat();
    push_prop(&mut structs, off_reg, &reg);
    push_be32(&mut structs, FDT_END_NODE);

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"soc");
    push_prop(&mut structs, off_addr_cells, &2u32.to_be_bytes());
    push_prop(&mut structs, off_size_cells, &2u32.to_be_bytes());

    push_be32(&mut structs, FDT_BEGIN_NODE);
    push_name(&mut structs, b"uart@10000000");
    push_prop(&mut structs, off_reg, &reg[..16]);
    push_be32(&mut structs, FDT_END_NODE);

    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END_NODE);
    push_be32(&mut structs, FDT_END);

    let header_size = 10 * 4;
    let mem_rsvmap_size = 16;
    let off_mem_rsvmap = header_size as u32;
    let off_dt_struct = (header_size + mem_rsvmap_size) as u32;
    let off_dt_strings = off_dt_struct + structs.len() as u32;
    let totalsize = off_dt_strings + strings.len() as u32;

    let mut dtb = Vec::new();
    push_be32(&mut dtb, FDT_MAGIC);
    push_be32(&mut dtb, totalsize);
    push_be32(&mut dtb, off_dt_struct);
    push_be32(&mut dtb, off_dt_strings);
    push_be32(&mut dtb, off_mem_rsvmap);
    push_be32(&mut dtb, 17);
    push_be32(&mut dtb, 16);
    push_be32(&mut dtb, 0);
    push_be32(&mut dtb, strings.len() as u32);
    push_be32(&mut dtb, structs.len() as u32);
    dtb.extend_from_slice(&[0; 16]);
    dtb.extend_from_slice(&structs);
    dtb.extend_from_slice(strings);
    dtb
}

#[test]
fn dice_node_regions() {
    let dtb = build_dice_dtb();
    let fdt = LinuxFdt::new(&dtb).unwrap();
    let dice = fdt.dice().unwrap();
    let region = dice.regions().unwrap().next().unwrap();

    assert_eq!(region.starting_address as usize, 0x1234_5000);
    assert_eq!(region.size, 0x1000);
}

#[test]
fn memreserve_and_reserved_memory_regions() {
    let dtb = build_reserved_memory_dtb();
    let fdt = LinuxFdt::new(&dtb).unwrap();

    let memreserve = fdt.mem_reservations().collect::<Vec<_>>();
    assert_eq!(memreserve.len(), 1);
    assert_eq!(memreserve[0].starting_address as usize, 0x8000_0000);
    assert_eq!(memreserve[0].size, 0x1000);

    let reserved = fdt.reserved_memory_regions().collect::<Vec<_>>();
    assert_eq!(reserved.len(), 2);
    assert_eq!(reserved[0].starting_address as usize, 0x8100_0000);
    assert_eq!(reserved[0].size, 0x2000);
    assert_eq!(reserved[1].starting_address as usize, 0x8200_0000);
    assert_eq!(reserved[1].size, 0x3000);
}

#[test]
fn chosen_alias_and_memory_helpers() {
    let dtb = build_chosen_memory_dtb();
    let fdt = LinuxFdt::new(&dtb).unwrap();

    assert_eq!(fdt.chosen_bootargs(), Some("console=ttyS0"));
    assert_eq!(fdt.chosen_stdout_path(), Some("serial0:115200n8"));
    assert_eq!(fdt.root_compatible(), None);

    let uart = fdt.resolve_node("serial0").unwrap();
    assert_eq!(uart.name, "uart@10000000");

    let memory = fdt.memory_regions().collect::<Vec<_>>();
    assert_eq!(memory.len(), 1);
    assert_eq!(memory[0].starting_address as usize, 0x8000_0000);
    assert_eq!(memory[0].size, 0x4000_0000);
}

#[test]
fn crosvm_dtb_parses_nodes_after_chosen() {
    let fdt = setup_crosvm();
    let names = fdt.all_nodes().map(|node| node.name).collect::<Vec<_>>();

    let chosen = names.iter().position(|&name| name == "chosen").unwrap();
    let reserved = names
        .iter()
        .position(|&name| name == "reserved-memory")
        .unwrap_or_else(|| panic!("reserved-memory missing, nodes={names:?}"));
    let cpus = names
        .iter()
        .position(|&name| name == "cpus")
        .unwrap_or_else(|| panic!("cpus missing, nodes={names:?}"));
    let intc = names
        .iter()
        .position(|&name| name == "intc")
        .unwrap_or_else(|| panic!("intc missing, nodes={names:?}"));
    let timer = names
        .iter()
        .position(|&name| name == "timer")
        .unwrap_or_else(|| panic!("timer missing, nodes={names:?}"));
    let pci = names
        .iter()
        .position(|&name| name == "pci")
        .unwrap_or_else(|| panic!("pci missing, nodes={names:?}"));

    assert!(reserved > chosen);
    assert!(cpus > reserved);
    assert!(intc > cpus);
    assert!(timer > intc);
    assert!(pci > timer);
}

#[test]
fn crosvm_dtb_exposes_both_dice_nodes() {
    let fdt = setup_crosvm();

    let dice = fdt.dice().unwrap().regions().unwrap().next().unwrap();
    assert_eq!(dice.starting_address as usize, 0x7fe2_3000);
    assert_eq!(dice.size, 0x1000);

    let reserved_dice = fdt
        .find_node("/reserved-memory/dice")
        .unwrap_or_else(|| panic!("reserved-memory dice missing"))
        .reg()
        .unwrap()
        .next()
        .unwrap();
    assert_eq!(reserved_dice.starting_address as usize, 0x7fe2_3000);
    assert_eq!(reserved_dice.size, 0x1000);

    let reserved = fdt.reserved_memory_regions().collect::<Vec<_>>();
    assert!(reserved.iter().any(|region| {
        region.starting_address as usize == 0x7fe2_3000 && region.size == 0x1000
    }));
}
