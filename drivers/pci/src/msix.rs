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

#[cfg(target_arch = "x86_64")]
use core::ptr::NonNull;

#[cfg(target_arch = "x86_64")]
use tock_registers::interfaces::{ReadWriteable, Readable, Writeable};
use tock_registers::{register_bitfields, registers::ReadWrite};

use super::{Command, ConfigurationAccess, DeviceFunction, PciConfigAccess, PciRoot};

/// PCI MSI-X capability ID.
pub const MSIX_CAP_ID: u8 = 0x11;

pub const PCI_BAR_COUNT: u8 = 6;
const MSG_CTRL_TABLE_SIZE_MASK: u16 = 0x07FF;
const MSG_CTRL_FUNCTION_MASK: u16 = 1 << 14;
const MSG_CTRL_MSIX_ENABLE: u16 = 1 << 15;
const MSIX_ENABLE: u32 = (MSG_CTRL_MSIX_ENABLE as u32) << 16;
const MSIX_FUNCTION_MASK: u32 = (MSG_CTRL_FUNCTION_MASK as u32) << 16;
const MAX_MSIX_TABLE_ENTRIES: u16 = 2048;
const STATUS_COMMAND_OFFSET: u16 = 0x04;

/// Parsed MSI-X capability information for a PCI device.
#[derive(Debug, Clone, Copy)]
pub struct MsixCapability {
    /// Offset of the MSI-X capability in PCI config space.
    pub cap_offset: u16,
    /// Number of MSI-X table entries (Message Control table_size field + 1).
    pub table_size: u16,
    /// Whether MSI-X Enable was set when this capability was discovered.
    pub enabled: bool,
    /// Whether Function Mask was set when this capability was discovered.
    pub function_masked: bool,
    /// BAR index containing the MSI-X table.
    pub table_bar: u8,
    /// Byte offset of the MSI-X table within the BAR (8-byte aligned).
    pub table_offset: u32,
    /// BAR index containing the Pending Bit Array.
    pub pba_bar: u8,
    /// Byte offset of the PBA within the BAR (8-byte aligned).
    pub pba_offset: u32,
}

register_bitfields! {u32,
    MSIX_VECTOR_CTRL [
        /// Vector mask bit: 1 = masked, 0 = unmasked.
        MASK OFFSET(0) NUMBITS(1) []
    ]
}

/// A single MSI-X table entry, as defined by the PCIe spec.
#[repr(C)]
pub struct MsixTableEntry {
    msg_addr_lo: ReadWrite<u32>,
    msg_addr_hi: ReadWrite<u32>,
    msg_data: ReadWrite<u32>,
    vector_ctrl: ReadWrite<u32, MSIX_VECTOR_CTRL::Register>,
}

const _: () = {
    assert!(core::mem::size_of::<MsixTableEntry>() == 16);
    assert!(core::mem::align_of::<MsixTableEntry>() == 4);
};

#[cfg(target_arch = "x86_64")]
impl MsixTableEntry {
    fn mask(&self) {
        self.vector_ctrl.modify(MSIX_VECTOR_CTRL::MASK::SET);
    }

    fn unmask(&self) {
        self.vector_ctrl.modify(MSIX_VECTOR_CTRL::MASK::CLEAR);
    }

    fn write_message(&self, msg_addr: u32, msg_data: u32) {
        self.msg_addr_lo.set(msg_addr);
        self.msg_addr_hi.set(0);
        self.msg_data.set(msg_data);
    }

    fn flush_message_writes(&self) {
        let _ = self.msg_data.get();
    }
}

/// A mapped MSI-X table.
#[cfg(target_arch = "x86_64")]
pub struct MsixTable {
    base: NonNull<MsixTableEntry>,
    len: usize,
}

#[cfg(target_arch = "x86_64")]
impl MsixTable {
    /// Creates a wrapper around a mapped MSI-X table.
    ///
    /// # Safety
    ///
    /// `base` must point to mapped MSI-X table MMIO memory that contains at
    /// least `len` valid entries. The caller must ensure that this wrapper has
    /// unique ownership of the table register accesses while it is alive, and
    /// that the mapped table remains live for the full lifetime of the wrapper.
    pub unsafe fn new(base: NonNull<MsixTableEntry>, len: usize) -> Self {
        Self { base, len }
    }

    fn entry(&self, index: usize) -> Option<&MsixTableEntry> {
        if index >= self.len {
            return None;
        }

        // SAFETY: `index` was checked against `len`, and `new` requires `base`
        // to cover `len` MSI-X table entries.
        Some(unsafe { &*self.base.as_ptr().add(index) })
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
    let table_size = (msg_ctrl & MSG_CTRL_TABLE_SIZE_MASK) + 1;
    let enabled = msg_ctrl & MSG_CTRL_MSIX_ENABLE != 0;
    let function_masked = msg_ctrl & MSG_CTRL_FUNCTION_MASK != 0;

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
        enabled,
        function_masked,
        table_bar,
        table_offset,
        pba_bar,
        pba_offset,
    })
}

/// Validates that the MSI-X table and PBA fit in their BARs.
pub fn validate_msix_layout(
    cap: &MsixCapability,
    table_bar_size: u64,
    pba_bar_size: u64,
) -> Result<(), &'static str> {
    if cap.table_bar >= PCI_BAR_COUNT {
        return Err("invalid MSI-X table BAR index");
    }
    if cap.pba_bar >= PCI_BAR_COUNT {
        return Err("invalid MSI-X PBA BAR index");
    }
    if cap.table_size == 0 || cap.table_size > MAX_MSIX_TABLE_ENTRIES {
        return Err("invalid MSI-X table size");
    }

    let table_bytes = u64::from(cap.table_size) * core::mem::size_of::<MsixTableEntry>() as u64;
    let table_end = u64::from(cap.table_offset)
        .checked_add(table_bytes)
        .ok_or("MSI-X table size overflow")?;
    if table_end > table_bar_size {
        return Err("MSI-X table outside BAR");
    }

    let pba_bytes = u64::from(cap.table_size).div_ceil(64) * 8;
    let pba_end = u64::from(cap.pba_offset)
        .checked_add(pba_bytes)
        .ok_or("MSI-X PBA size overflow")?;
    if pba_end > pba_bar_size {
        return Err("MSI-X PBA outside BAR");
    }

    Ok(())
}

/// Enables MSI-X with Function Mask set and disables legacy INTx.
pub fn prepare_msix<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    config: &mut PciConfigAccess,
    bdf: DeviceFunction,
    cap: &MsixCapability,
) {
    // The MSI-X capability is always 4-byte aligned. The 32-bit word at
    // cap_offset contains: [ID | Next | MsgCtrl-lo | MsgCtrl-hi].
    // Message Control is in bits 31:16 of that word.
    // The MSI-X Enable bit is bit 15 of Message Control = bit 31 of the word.
    // Function Mask is bit 14 of Message Control = bit 30 of the word.
    let word = config.read_word(bdf, cap.cap_offset);
    config.write_word(bdf, cap.cap_offset, word | MSIX_ENABLE | MSIX_FUNCTION_MASK);

    // Disable legacy INTx to avoid spurious interrupts on the shared IRQ line.
    let (_, cmd) = root.get_status_command(bdf);
    root.set_command(bdf, cmd | Command::INTERRUPT_DISABLE);
}

/// Clears the MSI-X Function Mask after the device vectors are ready.
pub fn activate_msix(config: &mut PciConfigAccess, bdf: DeviceFunction, cap: &MsixCapability) {
    let word = config.read_word(bdf, cap.cap_offset);
    config.write_word(bdf, cap.cap_offset, word & !MSIX_FUNCTION_MASK);
}

/// Disables MSI-X and re-enables legacy INTx delivery.
pub fn disable_msix<C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    config: &mut PciConfigAccess,
    bdf: DeviceFunction,
    cap: &MsixCapability,
) {
    restore_msix_message_control(config, bdf, cap);

    let (_, cmd) = root.get_status_command(bdf);
    root.set_command(bdf, cmd & !Command::INTERRUPT_DISABLE);
}

/// Disables MSI-X using direct config-space access.
///
/// This is used after the PCI root object is no longer available, for example
/// when tearing down a transport that already owns its copied config accessor.
pub fn disable_msix_with_config(
    config: &mut PciConfigAccess,
    bdf: DeviceFunction,
    cap: &MsixCapability,
) {
    restore_msix_message_control(config, bdf, cap);

    let status_command = config.read_word(bdf, STATUS_COMMAND_OFFSET);
    let command = Command::from_bits_truncate(status_command as u16) & !Command::INTERRUPT_DISABLE;
    // PCI Status bits are write-one-to-clear. Write zero to the upper Status
    // half while updating the lower Command half.
    config.write_word(bdf, STATUS_COMMAND_OFFSET, u32::from(command.bits()));
}

fn restore_msix_message_control(
    config: &mut PciConfigAccess,
    bdf: DeviceFunction,
    cap: &MsixCapability,
) {
    let mut word = config.read_word(bdf, cap.cap_offset) & !(MSIX_ENABLE | MSIX_FUNCTION_MASK);
    if cap.enabled {
        word |= MSIX_ENABLE;
    }
    if cap.function_masked {
        word |= MSIX_FUNCTION_MASK;
    }
    config.write_word(bdf, cap.cap_offset, word);
}

/// Configures a single MSI-X table entry for x86_64.
///
/// Writes the x86 MSI message address (targeting the given APIC) and the
/// message data (the CPU interrupt vector), then unmasks the entry.
///
/// # Arguments
///
/// * `table` - Mapped MSI-X table.
/// * `index` - Table entry index (0-based).
/// * `cpu_vector` - CPU interrupt vector number to deliver (e.g. 0x40..0xEF).
/// * `dest_apic_id` - APIC ID of the target CPU (usually the boot CPU, 0).
///
/// Returns `None` if `index` is outside the mapped MSI-X table.
#[cfg(target_arch = "x86_64")]
pub fn configure_msix_entry(
    table: &MsixTable,
    index: usize,
    cpu_vector: u8,
    dest_apic_id: u8,
) -> Option<()> {
    assert!(
        cpu_vector >= 32,
        "invalid CPU vector {} for MSI-X",
        cpu_vector
    );
    let entry = table.entry(index)?;

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

    entry.mask();
    entry.write_message(msg_addr, msg_data);
    // Read back from the MSI-X table to flush posted MMIO writes before
    // unmasking the entry.
    entry.flush_message_writes();
    entry.unmask();

    Some(())
}
