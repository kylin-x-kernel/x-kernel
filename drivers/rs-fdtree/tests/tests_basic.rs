// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

static DTB_DATA: &[u8] = include_bytes!("../dtb/test.dtb");

use rs_fdtree::LinuxFdt;

fn setup() -> LinuxFdt<'static> {
    LinuxFdt::new(DTB_DATA).unwrap()
}

#[test]
fn parse_fdt() {
    let fdt = setup();
    assert_eq!(fdt.total_size(), DTB_DATA.len());
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
    push_name(&mut structs, b"chosen");
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

#[test]
fn dice_node_regions() {
    let dtb = build_dice_dtb();
    let fdt = LinuxFdt::new(&dtb).unwrap();
    let dice = fdt.dice().unwrap();
    let region = dice.regions().unwrap().next().unwrap();

    assert_eq!(region.starting_address as usize, 0x1234_5000);
    assert_eq!(region.size, 0x1000);
}
