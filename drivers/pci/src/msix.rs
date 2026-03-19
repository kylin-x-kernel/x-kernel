// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! PCI MSI-X capability support.
//!
//! MSI-X provides per-device interrupt vectors that are:
//! - Edge-triggered (MSI write is one-shot), no level-triggered re-fire issues
//! - Independent per vector, no shared IRQ lines
//! - Dynamically allocated, no hardcoded IRQ numbers
//! - Direct to CPU, no IO-APIC routing needed

use super::{Command, ConfigurationAccess, DeviceFunction, PciConfigAccess, PciRoot};

/// PCI MSI-X capability ID.
pub const MSIX_CAP_ID: u8 = 0x11;

/// Parsed MSI-X capability information for a PCI device.
#[derive(Debug, Clone)]
pub struct MsixCapability {
    /// Offset of the MSI-X capability in PCI config space.
    pub cap_offset: u16,
    /// Number of MSI-X table entries (Message Control table_size field + 1).
    pub table_size: u16,
    /// BAR index containing the MSI-X table.
    pub table_bar: u8,
    /// Byte offset of the MSI-X table within the BAR (8-byte aligned).
    pub table_offset: u32,
    /// BAR index containing the Pending Bit Array.
    pub pba_bar: u8,
    /// Byte offset of the PBA within the BAR (8-byte aligned).
    pub pba_offset: u32,
}

/// A single MSI-X table entry (16 bytes, as defined by the PCIe spec).
#[derive(Debug)]
#[repr(C)]
pub struct MsixTableEntry {
    /// Lower 32 bits of the MSI message address.
    pub msg_addr_lo: u32,
    /// Upper 32 bits of the MSI message address.
    pub msg_addr_hi: u32,
    /// MSI message data (interrupt vector and delivery mode).
    pub msg_data: u32,
    /// Vector control: bit 0 = mask (1 = masked, 0 = unmasked).
    pub vector_ctrl: u32,
}

impl MsixTableEntry {
    const MASKED: u32 = 1;

    /// Masks this MSI-X table entry (disables the interrupt).
    ///
    /// # Safety
    ///
    /// The pointer must be valid and properly aligned MMIO memory.
    pub unsafe fn mask(entry: *mut Self) {
        unsafe {
            let ctrl = core::ptr::addr_of!((*entry).vector_ctrl);
            let val = ctrl.read_volatile();
            (ctrl as *mut u32).write_volatile(val | Self::MASKED);
        }
    }

    /// Unmasks this MSI-X table entry (enables the interrupt).
    ///
    /// # Safety
    ///
    /// The pointer must be valid and properly aligned MMIO memory.
    pub unsafe fn unmask(entry: *mut Self) {
        unsafe {
            let ctrl = core::ptr::addr_of!((*entry).vector_ctrl);
            let val = ctrl.read_volatile();
            (ctrl as *mut u32).write_volatile(val & !Self::MASKED);
        }
    }
}

/// Scans the PCI capability list and returns the parsed MSI-X capability, if present.
///
/// Returns `None` if the device does not advertise MSI-X (capability ID 0x11).
pub fn find_msix_capability<C: ConfigurationAccess>(
    root: &PciRoot<C>,
    config: &PciConfigAccess,
    bdf: DeviceFunction,
) -> Option<MsixCapability> {
    let cap_info = root.capabilities(bdf).find(|cap| cap.id == MSIX_CAP_ID)?;

    let cap_offset = cap_info.offset as u16;

    // Message Control is in cap_info.private_header (bytes 2–3 of capability).
    // Bits 10:0 encode table_size - 1.
    let msg_ctrl = cap_info.private_header;
    let table_size = (msg_ctrl & 0x7FF) + 1;

    // Table BIR/Offset register is at cap_offset + 4.
    let table_bir_word = config.read_word(bdf, cap_offset + 4);
    let table_bar = (table_bir_word & 0x7) as u8;
    let table_offset = table_bir_word & !0x7;

    // PBA BIR/Offset register is at cap_offset + 8.
    let pba_bir_word = config.read_word(bdf, cap_offset + 8);
    let pba_bar = (pba_bir_word & 0x7) as u8;
    let pba_offset = pba_bir_word & !0x7;

    Some(MsixCapability {
        cap_offset,
        table_size,
        table_bar,
        table_offset,
        pba_bar,
        pba_offset,
    })
}

/// Enables MSI-X for the given device.
///
/// Sets the MSI-X Enable bit in Message Control and disables legacy INTx by
/// setting `Command::INTERRUPT_DISABLE`.
pub fn enable_msix<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    config: &mut PciConfigAccess,
    bdf: DeviceFunction,
    cap: &MsixCapability,
) {
    // The MSI-X capability is always 4-byte aligned. The 32-bit word at
    // cap_offset contains: [ID | Next | MsgCtrl-lo | MsgCtrl-hi].
    // Message Control is in bits 31:16 of that word.
    // The MSI-X Enable bit is bit 15 of Message Control = bit 31 of the word.
    let word = config.read_word(bdf, cap.cap_offset);
    config.write_word(bdf, cap.cap_offset, word | (1u32 << 31));

    // Disable legacy INTx to avoid spurious interrupts on the shared IRQ line.
    let (_, cmd) = root.get_status_command(bdf);
    root.set_command(bdf, cmd | Command::INTERRUPT_DISABLE);
}

/// Configures a single MSI-X table entry for x86_64.
///
/// Writes the x86 MSI message address (targeting the given APIC) and the
/// message data (the CPU interrupt vector), then unmasks the entry.
///
/// # Arguments
///
/// * `table_base` - Virtual pointer to the start of the MSI-X table MMIO region.
/// * `index`      - Table entry index (0-based).
/// * `cpu_vector` - CPU interrupt vector number to deliver (e.g. 0x40..0xEF).
/// * `dest_apic_id` - APIC ID of the target CPU (usually the boot CPU, 0).
///
/// # Safety
///
/// `table_base` must be a valid, mapped MMIO pointer to the MSI-X table. The
/// table must have at least `index + 1` entries.
#[cfg(target_arch = "x86_64")]
pub unsafe fn configure_msix_entry(
    table_base: *mut MsixTableEntry,
    index: usize,
    cpu_vector: u8,
    dest_apic_id: u8,
) {
    // x86 MSI address format:
    //   Bits 31:20 = 0xFEE (fixed)
    //   Bits 19:12 = Destination ID (APIC ID)
    //   Bits 11:4  = reserved/0
    //   Bit  3     = Redirect Hint (0 = directed)
    //   Bit  2     = Destination Mode (0 = physical)
    //   Bits 1:0   = reserved/0
    let msg_addr = 0xFEE0_0000u32 | ((dest_apic_id as u32) << 12);

    // x86 MSI data format:
    //   Bits 7:0   = Vector
    //   Bits 10:8  = Delivery mode (000 = Fixed)
    //   Bit  14    = Level (0 for edge)
    //   Bit  15    = Trigger mode (0 = edge)
    let msg_data = cpu_vector as u32;

    unsafe {
        let entry = table_base.add(index);
        core::ptr::addr_of_mut!((*entry).msg_addr_lo).write_volatile(msg_addr);
        core::ptr::addr_of_mut!((*entry).msg_addr_hi).write_volatile(0);
        core::ptr::addr_of_mut!((*entry).msg_data).write_volatile(msg_data);
        // Unmask the entry last so the vector is fully configured before delivery.
        MsixTableEntry::unmask(entry);
    }
}
