use super::object::WrappedObject;
use crate::aml::{AmlError, Operation, object::Object};
use alloc::{string::String, vec::Vec};
use bit_field::BitField;
use byteorder::{ByteOrder, LittleEndian};
use core::mem;

#[derive(Debug, PartialEq, Eq)]
pub enum Resource {
    Irq(IrqDescriptor),
    AddressSpace(AddressSpaceDescriptor),
    MemoryRange(MemoryRangeDescriptor),
    IOPort(IOPortDescriptor),
    Dma(DMADescriptor),
}

/// One framed resource-template item.
///
/// The body is the descriptor payload without its small-item or large-item
/// header. Framing validity is checked while iterating [`ResourceDescriptors`];
/// use [`ResourceDescriptor::decode`] to interpret a known item type. Item
/// types this decoder does not interpret are still valid framing and decode to
/// `None`, so a caller scanning for one fact does not reject an unrelated
/// descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceDescriptor<'a> {
    header: u8,
    body: &'a [u8],
}

impl<'a> ResourceDescriptor<'a> {
    /// Returns the raw item header byte.
    pub fn header(&self) -> u8 {
        self.header
    }

    /// Returns whether this is a large item (bit 7 of the header is set).
    pub fn is_large(&self) -> bool {
        self.header.get_bit(7)
    }

    /// Returns the item type with the framing bits removed.
    pub fn resource_type(&self) -> u8 {
        if self.is_large() {
            self.header.get_bits(0..7)
        } else {
            self.header.get_bits(3..=6)
        }
    }

    /// Returns the descriptor body without its header.
    pub fn body(&self) -> &'a [u8] {
        self.body
    }

    /// Decodes a descriptor type this library interprets.
    ///
    /// Returns `Ok(None)` for a well-framed item type that this decoder does not
    /// interpret. Returns [`AmlError::InvalidResourceDescriptor`] when the body
    /// of an interpreted type is malformed.
    pub fn decode(&self) -> Result<Option<Resource>, AmlError> {
        if self.is_large() {
            match self.resource_type() {
                0x06 => fixed_memory_descriptor(self.body).map(Some),
                0x07 => address_space_descriptor::<u32>(self.body).map(Some),
                0x08 => address_space_descriptor::<u16>(self.body).map(Some),
                0x09 => extended_interrupt_descriptor(self.body).map(Some),
                0x0a => address_space_descriptor::<u64>(self.body).map(Some),
                _ => Ok(None),
            }
        } else {
            match self.resource_type() {
                0x04 => irq_format_descriptor(self.body).map(Some),
                0x05 => dma_format_descriptor(self.body).map(Some),
                0x08 => io_port_descriptor(self.body).map(Some),
                _ => Ok(None),
            }
        }
    }
}

/// A fallible iterator over the framed items of a resource template.
///
/// Iteration stops at the end tag or at the end of the buffer. The end tag is
/// reported separately by [`ResourceDescriptors::end_tag_seen`]; a truncated
/// item is reported as an error and terminates iteration.
pub struct ResourceDescriptors<'a> {
    remaining: &'a [u8],
    end_tag_seen: bool,
    finished: bool,
}

impl<'a> ResourceDescriptors<'a> {
    /// Returns whether an end tag terminated the template.
    pub fn end_tag_seen(&self) -> bool {
        self.end_tag_seen
    }
}

impl<'a> Iterator for ResourceDescriptors<'a> {
    type Item = Result<ResourceDescriptor<'a>, AmlError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished || self.remaining.is_empty() {
            return None;
        }

        let header = self.remaining[0];
        if header == 0x79 {
            // The EndTag is a one-byte small-item body: the checksum byte is
            // still required even though no caller interprets it here. A bare
            // `0x79` is a truncated template, not a terminator.
            if self.remaining.get(1).is_none() {
                self.finished = true;
                return Some(Err(AmlError::InvalidResourceDescriptor));
            }
            self.end_tag_seen = true;
            self.finished = true;
            return None;
        }

        let (body_offset, body_len) = if header.get_bit(7) {
            let Some(length_bytes) = self.remaining.get(1..3) else {
                self.finished = true;
                return Some(Err(AmlError::InvalidResourceDescriptor));
            };
            (3usize, usize::from(LittleEndian::read_u16(length_bytes)))
        } else {
            (1usize, usize::from(header.get_bits(0..=2)))
        };

        let Some(end) = body_offset.checked_add(body_len) else {
            self.finished = true;
            return Some(Err(AmlError::InvalidResourceDescriptor));
        };
        let Some(descriptor) = self.remaining.get(..end) else {
            self.finished = true;
            return Some(Err(AmlError::InvalidResourceDescriptor));
        };
        let body = &descriptor[body_offset..];
        self.remaining = &self.remaining[end..];

        Some(Ok(ResourceDescriptor { header, body }))
    }
}

/// Iterates the framed items of a `ResourceTemplate` buffer.
pub fn resource_descriptors(bytes: &[u8]) -> ResourceDescriptors<'_> {
    ResourceDescriptors {
        remaining: bytes,
        end_tag_seen: false,
        finished: false,
    }
}

/// Parse a `ResourceDescriptor` buffer into a list of resources.
///
/// Item types this decoder does not interpret are skipped. The end tag is
/// consumed but not required by this convenience function; callers that need to
/// distinguish a truncated template should use [`resource_descriptors`].
pub fn resource_descriptor_list(descriptor: WrappedObject) -> Result<Vec<Resource>, AmlError> {
    if let Object::Buffer(ref bytes) = *descriptor {
        let mut descriptors = Vec::new();
        for descriptor in resource_descriptors(bytes) {
            if let Some(descriptor) = descriptor?.decode()? {
                descriptors.push(descriptor);
            }
        }
        Ok(descriptors)
    } else {
        Err(AmlError::InvalidOperationOnObject { op: Operation::ParseResource, typ: descriptor.typ() })
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum InterruptTrigger {
    Edge,
    Level,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum InterruptPolarity {
    ActiveHigh,
    ActiveLow,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum AddressSpaceResourceType {
    MemoryRange,
    IORange,
    BusNumberRange,
    /// A hardware-vendor-defined resource type (192..=255).
    VendorDefined,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
pub enum AddressSpaceDecodeType {
    Additive,
    Subtractive,
}

#[derive(Debug, PartialEq, Eq)]
pub struct AddressSpaceDescriptor {
    pub resource_type: AddressSpaceResourceType,
    pub is_maximum_address_fixed: bool,
    pub is_minimum_address_fixed: bool,
    pub decode_type: AddressSpaceDecodeType,

    pub granularity: u64,
    pub address_range: (u64, u64),
    pub translation_offset: u64,
    pub length: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MemoryRangeDescriptor {
    FixedLocation { is_writable: bool, base_address: u32, range_length: u32 },
}

fn fixed_memory_descriptor(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * -- 32-bit Fixed Memory Descriptor ---
     * Offset     Field Name                              Definition
     * Byte 0     32-bit Fixed Memory Range Descriptor    Value = 0x86 (10000110B) – Type = 1, Large item name = 0x06
     * Byte 1     Length, bits [7:0]                      Value = 0x09 (9)
     * Byte 2     Length, bits [15:8]                     Value = 0x00
     * Byte 3     Information                             This field provides extra information about this memory.
     *                                                    Bit [7:1]   Ignored
     *                                                    Bit [0]     Write status, _RW
     *                                                        1  writeable (read/write)
     *                                                        0  non-writeable (read-only)
     * Byte 4     Range base address, _BAS bits [7:0]     Address bits [7:0] of the base memory address for which the card may be configured.
     * Byte 5     Range base address, _BAS bits [15:8]    Address bits [15:8] of the base memory address for which the card may be configured.
     * Byte 6     Range base address, _BAS bits [23:16]   Address bits [23:16] of the base memory address for which the card may be configured.
     * Byte 7     Range base address, _BAS bits [31:24]   Address bits [31:24] of the base memory address for which the card may be configured.
     * Byte 8     Range length, _LEN bits [7:0]           This field contains bits [7:0] of the memory range length. The range length provides the length of the memory range in 1-byte blocks.
     * Byte 9     Range length, _LEN bits [15:8]          This field contains bits [15:8] of the memory range length. The range length provides the length of the memory range in 1-byte blocks.
     * Byte 10    Range length, _LEN bits [23:16]         This field contains bits [23:16] of the memory range length. The range length provides the length of the memory range in 1-byte blocks.
     * Byte 11    Range length, _LEN bits [31:24]         This field contains bits [31:24] of the memory range length. The range length provides the length of the memory range in 1-byte blocks.
     */
    if body.len() != 9 {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    let information = body[0];
    let is_writable = information.get_bit(0);

    let base_address = LittleEndian::read_u32(&body[1..5]);
    let range_length = LittleEndian::read_u32(&body[5..9]);

    Ok(Resource::MemoryRange(MemoryRangeDescriptor::FixedLocation { is_writable, base_address, range_length }))
}

fn address_space_descriptor<T>(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * WORD Address Space Descriptor Definition
     * Note: The definitions for DWORD and QWORD are the same other than the width of the address fields.
     *
     * Offset  Field Name                                   Definition
     * Byte 0  WORD Address Space Descriptor                Value = 0x88 (10001000B) – Type = 1, Large item name = 0x08
     * Byte 1  Length, bits [7:0]                           Variable length, minimum value = 0x0D (13)
     * Byte 2  Length, bits [15:8]                          Variable length, minimum value = 0x00
     * Byte 3  Resource Type                                Indicates which type of resource this descriptor describes. Defined values are:
     *                                                        0         Memory range
     *                                                        1         I/O range
     *                                                        2         Bus number range
     *                                                        3–191     Reserved
     *                                                        192-255   Hardware Vendor Defined
     * Byte 4  General Flags                                Flags that are common to all resource types:
     *                                                        Bits [7:4]   Reserved (must be 0)
     *                                                        Bit [3]     Max Address Fixed, _MAF:
     *                                                          1  The specified maximum address is fixed
     *                                                          0  The specified maximum address is not fixed
     *                                                             and can be changed
     *                                                        Bit [2]      Min Address Fixed,_MIF:
     *                                                          1   The specified minimum address is fixed
     *                                                          0   The specified minimum address is not fixed
     *                                                              and can be changed
     *                                                        Bit [1]      Decode Type, _DEC:
     *                                                          1   This bridge subtractively decodes this address          (top level bridges only)
     *                                                          0   This bridge positively decodes this address
     *                                                        Bit [0]      Ignored
     * Byte 5  Type Specific Flags                           Flags that are specific to each resource type. The meaning of the flags in this field depends on the value of the Resource Type field (see above).
     * Byte 6  Address space granularity, _GRA bits[7:0]     A set bit in this mask means that this bit is decoded. All bits less significant than the most significant set bit must be set. (In other words, the value of the full Address Space Granularity field (all 16 bits) must be a number (2n-1).
     * Byte 7  Address space granularity, _GRA bits[15:8]
     * Byte 8  Address range minimum, _MIN, bits [7:0]       For bridges that translate addresses, this is the address space on the secondary side of the bridge.
     * Byte 9  Address range minimum, _MIN, bits [15:8]
     * Byte 10 Address range maximum, _MAX, bits [7:0]       For bridges that translate addresses, this is the address space on the secondary side of the bridge.
     * Byte 11 Address range maximum, _MAX, bits [15:8]
     * Byte 12 Address Translation offset, _TRA, bits [7:0]  For bridges that translate addresses across the bridge, this is the offset that must be added to the address on the secondary side to obtain the address on the primary side. Non-bridge devices must list 0 for all Address Translation offset bits.
     * Byte 13 Address Translation offset, _TRA, bits [15:8]
     * Byte 14 Address Length, _LEN, bits [7:0]
     * Byte 15 Address Length, _LEN, bits [15:8]
     * Byte 16 Resource Source Index (Optional)              Only present if Resource Source (below) is present. This field gives an index to the specific resource descriptor that this device consumes from in the current resource template for the device object pointed to in Resource Source.
     * String  Resource Source (Optional)                    If present, the device that uses this descriptor consumes its resources from the resources produced by the named device object. If not present, the device consumes its resources out of a global pool. If not present, the device consumes this resource from its hierarchical parent.
     *
     * The body fields begin at body offset 0, which is descriptor byte 3. Each
     * address field is `size` bytes wide, so the five fields occupy
     * `3 + 5 * size` bytes of the body.
     */
    let size = mem::size_of::<T>();

    if body.len() < 3 + size * 5 {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    let resource_type = match body[0] {
        0 => AddressSpaceResourceType::MemoryRange,
        1 => AddressSpaceResourceType::IORange,
        2 => AddressSpaceResourceType::BusNumberRange,
        3..=191 => return Err(AmlError::InvalidResourceDescriptor),
        192..=255 => AddressSpaceResourceType::VendorDefined,
    };

    let general_flags = body[1];
    let is_maximum_address_fixed = general_flags.get_bit(3);
    let is_minimum_address_fixed = general_flags.get_bit(2);
    let decode_type = if general_flags.get_bit(1) {
        AddressSpaceDecodeType::Subtractive
    } else {
        AddressSpaceDecodeType::Additive
    };

    // Body: type, general flags, type flags, granularity, minimum, maximum,
    // translation, length. Each address field is `size` bytes wide.
    let granularity = read_uint(body, 3, size);
    let address_range_min = read_uint(body, 3 + size, size);
    let address_range_max = read_uint(body, 3 + 2 * size, size);
    let translation_offset = read_uint(body, 3 + 3 * size, size);
    let length = read_uint(body, 3 + 4 * size, size);

    Ok(Resource::AddressSpace(AddressSpaceDescriptor {
        resource_type,
        is_maximum_address_fixed,
        is_minimum_address_fixed,
        decode_type,
        granularity,
        address_range: (address_range_min, address_range_max),
        translation_offset,
        length,
    }))
}

fn read_uint(bytes: &[u8], offset: usize, size: usize) -> u64 {
    let end = offset + size;
    LittleEndian::read_uint(&bytes[offset..end], size)
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct IrqDescriptor {
    pub is_consumer: bool,
    pub trigger: InterruptTrigger,
    pub polarity: InterruptPolarity,
    pub is_shared: bool,
    pub is_wake_capable: bool,
    /// The interrupt numbers named by the descriptor. A small IRQ descriptor
    /// encodes these as a mask; an extended descriptor lists them explicitly.
    /// At least one number is always present.
    pub irqs: Vec<u32>,
    /// The optional `ResourceSource` suffix carried by an extended interrupt
    /// descriptor. Small IRQ descriptors cannot carry one.
    pub source: Option<ResourceSource>,
}

/// The optional `ResourceSource` suffix of an extended interrupt descriptor.
///
/// The suffix names another device's resource template and an index into it.
/// Whether a consumer supports it is routing policy, not descriptor validity,
/// so the decoder retains it as a fact.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ResourceSource {
    /// Index into the named device's resource template.
    pub index: u8,
    /// Null-terminated AML resource-source device path.
    pub name: String,
}

fn irq_format_descriptor(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * IRQ Descriptor Definition
     *
     * Offset   Field Name
     * Byte 0   Value = 0x22 or 0x23 (0010001nB)– Type = 0, Small item name = 0x4, Length = 2 or 3
     * Byte 1   IRQ mask bits[7:0], _INT
     *          Bit [0] represents IRQ0, bit[1] is IRQ1, and so on.
     * Byte 2   IRQ mask bits[15:8], _INT
     *          Bit [0] represents IRQ8, bit[1] is IRQ9, and so on.
     * Byte 3   IRQ Information. Each bit, when set, indicates this device is capable of driving a certain type of interrupt.
     *          (Optional—if not included then assume edge sensitive, high true interrupts.)
     *          These bits can be used both for reporting and setting IRQ resources.
     *          Note: This descriptor is meant for describing interrupts that are connected to PIC-compatible interrupt
     *                controllers, which can only be programmed for Active-High-Edge-Triggered or Active-Low-Level-Triggered
     *                interrupts. Any other combination is invalid.
     *            Bit [7:6]  Reserved (must be 0)
     *            Bit [5]    Wake Capability, _WKC
     *              0x0 = Not Wake Capable: This interrupt is not capable of waking the system.
     *              0x1 = Wake Capable: This interrupt is capable of waking the system from a
     *                    low-power idle state or a system sleep state.
     *            Bit [4]    Interrupt Sharing, _SHR
     *              0x0 = Exclusive: This interrupt is not shared with other devices.
     *              0x1 = Shared: This interrupt is shared with other devices.
     *            Bit [3]    Interrupt Polarity, _LL
     *              0  Active-High – This interrupt is sampled when the signal is high, or true
     *              1  Active-Low – This interrupt is sampled when the signal is low, or false.
     *            Bit [2:1]  Ignored
     *            Bit [0]    Interrupt Mode, _HE
     *              0  Level-Triggered – Interrupt is triggered in response to signal in a low state.
     *              1  Edge-Triggered – Interrupt is triggered in response to a change in signal state from low to high.
     *
     * The mask is a bitmask: bit `n` names IRQ `n`. It is not the IRQ number.
     */
    let (mask, information) = match body {
        [low, high] => (u16::from_le_bytes([*low, *high]), None),
        [low, high, information] => (u16::from_le_bytes([*low, *high]), Some(*information)),
        _ => return Err(AmlError::InvalidResourceDescriptor),
    };
    let irqs: Vec<u32> = (0..16u32).filter(|bit| mask.get_bit(*bit as usize)).collect();
    if irqs.is_empty() {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    // Without the optional IRQ Information byte, the descriptor means edge
    // sensitive, active high, not shared and not wake capable.
    let is_wake_capable = information.is_some_and(|value| value.get_bit(5));
    let is_shared = information.is_some_and(|value| value.get_bit(4));
    let polarity = if information.is_some_and(|value| value.get_bit(3)) {
        InterruptPolarity::ActiveLow
    } else {
        InterruptPolarity::ActiveHigh
    };
    let trigger = match information {
        None => InterruptTrigger::Edge,
        Some(value) if value.get_bit(0) => InterruptTrigger::Edge,
        Some(_) => InterruptTrigger::Level,
    };

    Ok(Resource::Irq(IrqDescriptor {
        is_consumer: false, // small IRQ descriptors have no consumer bit
        trigger,
        polarity,
        is_shared,
        is_wake_capable,
        irqs,
        source: None,
    }))
}

#[derive(Debug, PartialEq, Eq)]
pub enum DMASupportedSpeed {
    CompatibilityMode,
    TypeA, // as described by the EISA
    TypeB,
    TypeF,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DMATransferTypePreference {
    _8BitOnly,
    _8And16Bit,
    _16Bit,
}

#[derive(Debug, PartialEq, Eq)]
pub struct DMADescriptor {
    pub channel_mask: u8,
    pub supported_speeds: DMASupportedSpeed,
    pub is_bus_master: bool,
    pub transfer_type_preference: DMATransferTypePreference,
}

pub fn dma_format_descriptor(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * DMA Descriptor Definition
     * Offset  Field Name
     * Byte 0  Value = 0x2A (00101010B) – Type = 0, Small item name = 0x5, Length = 2
     * Byte 1  DMA channel mask bits [7:0] (channels 0 – 7), _DMA
     *         Bit [0] is channel 0, etc.
     * Byte 2  Bit [7]           Reserved (must be 0)
     *         Bits [6:5]        DMA channel speed supported, _TYP
     *           00    Indicates compatibility mode
     *           01    Indicates Type A DMA as described in the EISA
     *           10    Indicates Type B DMA
     *           11    Indicates Type F
     *         Bits [4:3]        Ignored
     *         Bit [2]           Logical device bus master status, _BM
     *           0      Logical device is not a bus master
     *           1      Logical device is a bus master
     *         Bits [1:0]       DMA transfer type preference, _SIZ
     *           00    8-bit only
     *           01    8- and 16-bit
     *           10    16-bit only
     *           11    Reserved
     */
    if body.len() != 2 {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    let channel_mask = body[0];
    let options = body[1];
    let supported_speeds = match options.get_bits(5..=6) {
        0 => DMASupportedSpeed::CompatibilityMode,
        1 => DMASupportedSpeed::TypeA,
        2 => DMASupportedSpeed::TypeB,
        3 => DMASupportedSpeed::TypeF,
        _ => return Err(AmlError::InvalidResourceDescriptor),
    };
    let is_bus_master = options.get_bit(2);
    let transfer_type_preference = match options.get_bits(0..=1) {
        0 => DMATransferTypePreference::_8BitOnly,
        1 => DMATransferTypePreference::_8And16Bit,
        2 => DMATransferTypePreference::_16Bit,
        _ => return Err(AmlError::InvalidResourceDescriptor),
    };

    Ok(Resource::Dma(DMADescriptor { channel_mask, supported_speeds, is_bus_master, transfer_type_preference }))
}

#[derive(Debug, PartialEq, Eq)]
pub struct IOPortDescriptor {
    pub decodes_full_address: bool,
    pub memory_range: (u16, u16),
    pub base_alignment: u8,
    pub range_length: u8,
}

fn io_port_descriptor(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * I/O Port Descriptor Definition
     * Offset   Field Name                                  Definition
     * Byte 0   I/O Port Descriptor                         Value = 0x47 (01000111B) –
     *                                                      Type = 0, Small item name = 0x8, Length = 7
     * Byte 1   Information                                 Bits [7:1]     Reserved and must be 0
     *                                                      Bit [0]          (_DEC)
     *                                                        1    The logical device decodes 16-bit addresses
     *                                                        0    The logical device only decodes address bits[9:0]
     * Byte 2   Range minimum base address, _MIN bits[7:0]  Address bits [7:0] of the minimum base I/O address that the card may be configured for.
     * Byte 3   Range minimum base address, _MIN bits[15:8] Address bits [15:8] of the minimum base I/O address that the card may be configured for.
     * Byte 4   Range maximum base address, _MAX bits[7:0]  Address bits [7:0] of the maximum base I/O address that the card may be configured for.
     * Byte 5   Range maximum base address, _MAX bits[15:8] Address bits [15:8] of the maximum base I/O address that the card may be configured for.
     * Byte 6   Base alignment, _ALN                        Alignment for minimum base address, increment in 1-byte blocks.
     * Byte 7   Range length, _LEN                          The number of contiguous I/O ports requested.
     */
    if body.len() != 7 {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    let information = body[0];
    let decodes_full_address = information.get_bit(0);

    let memory_range_min = LittleEndian::read_u16(&body[1..3]);
    let memory_range_max = LittleEndian::read_u16(&body[3..5]);
    let memory_range = (memory_range_min, memory_range_max);

    let base_alignment = body[5];
    let range_length = body[6];

    Ok(Resource::IOPort(IOPortDescriptor { decodes_full_address, memory_range, base_alignment, range_length }))
}

fn extended_interrupt_descriptor(body: &[u8]) -> Result<Resource, AmlError> {
    /*
     * --- Extended Interrupt Descriptor ---
     * Byte 3 contains the Interrupt Vector Flags:
     *      Bit 0: 1 if device consumes the resource, 0 if it produces it
     *      Bit 1: 1 if edge-triggered, 0 if level-triggered
     *      Bit 2: 1 = active-low, 0 = active-high
     *      Bit 3: 1 if interrupt is shared with other devices
     *      Bit 4: 1 if this interrupt is capable of waking the system, 0 if it is not
     * Byte 4 contains the number of interrupt numbers that follow. When this descriptor is
     * returned from `_CRS` or send to `_SRS`, this field must be 1.
     *
     * From Byte 5 onwards, there are `n` interrupt numbers, each of which is encoded as a
     * 4-byte little-endian number.
     */
    if body.len() < 2 {
        return Err(AmlError::InvalidResourceDescriptor);
    }

    let flags = body[0];
    let number_of_interrupts = usize::from(body[1]);
    if number_of_interrupts == 0 {
        return Err(AmlError::InvalidResourceDescriptor);
    }
    let minimum_length = 2 + 4 * number_of_interrupts;
    if body.len() < minimum_length {
        return Err(AmlError::InvalidResourceDescriptor);
    }
    let irqs = body[2..minimum_length].chunks_exact(4).map(LittleEndian::read_u32).collect();
    let source = resource_source(&body[minimum_length..])?;

    Ok(Resource::Irq(IrqDescriptor {
        is_consumer: flags.get_bit(0),
        trigger: if flags.get_bit(1) { InterruptTrigger::Edge } else { InterruptTrigger::Level },
        polarity: if flags.get_bit(2) { InterruptPolarity::ActiveLow } else { InterruptPolarity::ActiveHigh },
        is_shared: flags.get_bit(3),
        is_wake_capable: flags.get_bit(4),
        irqs,
        source,
    }))
}

/// Decodes the optional `ResourceSourceIndex` + `ResourceSource` suffix.
///
/// The suffix is present only when the descriptor is longer than the vectors.
/// It is an index byte followed by a null-terminated string; a single trailing
/// byte is a truncated suffix, and trailing bytes after the terminator are
/// malformed because the descriptor length must describe exactly one suffix.
fn resource_source(extra: &[u8]) -> Result<Option<ResourceSource>, AmlError> {
    if extra.is_empty() {
        return Ok(None);
    }
    let Some((&index, string)) = extra.split_first() else {
        return Err(AmlError::InvalidResourceDescriptor);
    };
    let Some((&terminator, name)) = string.split_last() else {
        return Err(AmlError::InvalidResourceDescriptor);
    };
    if terminator != 0 || name.contains(&0) {
        return Err(AmlError::InvalidResourceDescriptor);
    }
    let name = core::str::from_utf8(name).map_err(|_| AmlError::InvalidResourceDescriptor)?;
    Ok(Some(ResourceSource { index, name: name.into() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Resource> {
        let mut descriptors = Vec::new();
        for descriptor in resource_descriptors(bytes) {
            if let Some(descriptor) = descriptor.unwrap().decode().unwrap() {
                descriptors.push(descriptor);
            }
        }
        descriptors
    }

    fn irq(bytes: &[u8]) -> IrqDescriptor {
        match decode(bytes).as_slice() {
            [Resource::Irq(descriptor)] => descriptor.clone(),
            other => panic!("expected one IRQ descriptor, got {other:?}"),
        }
    }

    #[test]
    fn test_parses_keyboard_crs() {
        let bytes: Vec<u8> = [
            // Generated from `iasl -l pc-bios_acpi-dsdt.asl`
            //
            //         315:                   IO (Decode16,
            //         316:                       0x0060,             // Range Minimum
            //         317:                       0x0060,             // Range Maximum
            //         318:                       0x01,               // Alignment
            //         319:                       0x01,               // Length
            //         320:                       )

            //    0000040A:  47 01 60 00 60 00 01 01     "G.`.`..."
            0x47, 0x01, 0x60, 0x00, 0x60, 0x00, 0x01, 0x01,
            //         321:                   IO (Decode16,
            //         322:                       0x0064,             // Range Minimum
            //         323:                       0x0064,             // Range Maximum
            //         324:                       0x01,               // Alignment
            //         325:                       0x01,               // Length
            //         326:                       )

            //    00000412:  47 01 64 00 64 00 01 01     "G.d.d..."
            0x47, 0x01, 0x64, 0x00, 0x64, 0x00, 0x01, 0x01,
            //         327:                   IRQNoFlags ()
            //         328:                       {1}

            //    0000041A:  22 02 00 ...............    "".."
            0x22, 0x02, 0x00, //    0000041D:  79 00 ..................    "y."
            0x79, 0x00,
        ]
        .to_vec();

        let value = Object::Buffer(bytes).wrap();
        let resources = resource_descriptor_list(value).unwrap();

        assert_eq!(
            resources,
            Vec::from([
                Resource::IOPort(IOPortDescriptor {
                    decodes_full_address: true,
                    memory_range: (0x60, 0x60),
                    base_alignment: 1,
                    range_length: 1
                }),
                Resource::IOPort(IOPortDescriptor {
                    decodes_full_address: true,
                    memory_range: (0x64, 0x64),
                    base_alignment: 1,
                    range_length: 1
                }),
                Resource::Irq(IrqDescriptor {
                    is_consumer: false,
                    trigger: InterruptTrigger::Edge,
                    polarity: InterruptPolarity::ActiveHigh,
                    is_shared: false,
                    is_wake_capable: false,
                    irqs: Vec::from([1]),
                    source: None,
                })
            ])
        );
    }

    #[test]
    fn test_pci_crs() {
        let bytes: Vec<u8> = [
            // Generated from `iasl -l pc-bios_acpi-dsdt.asl`
            //
            //      98:               WordBusNumber (ResourceProducer, MinFixed, MaxFixed, PosDecode,
            //      99:                   0x0000,             // Granularity
            //     100:                   0x0000,             // Range Minimum
            //     101:                   0x00FF,             // Range Maximum
            //     102:                   0x0000,             // Translation Offset
            //     103:                   0x0100,             // Length
            //     104:                   ,, )

            // 000000F3:  88 0D 00 02 0C 00 00 00     "........"
            // 000000FB:  00 00 FF 00 00 00 00 01     "........"
            0x88, 0x0D, 0x00, 0x02, 0x0C, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x01,
            //     105:               IO (Decode16,
            //     106:                   0x0CF8,             // Range Minimum
            //     107:                   0x0CF8,             // Range Maximum
            //     108:                   0x01,               // Alignment
            //     109:                   0x08,               // Length
            //     110:                   )

            // 00000103:  47 01 F8 0C F8 0C 01 08     "G......."
            0x47, 0x01, 0xF8, 0x0C, 0xF8, 0x0C, 0x01, 0x08,
            //     111:               WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
            //     112:                   0x0000,             // Granularity
            //     113:                   0x0000,             // Range Minimum
            //     114:                   0x0CF7,             // Range Maximum
            //     115:                   0x0000,             // Translation Offset
            //     116:                   0x0CF8,             // Length
            //     117:                   ,, , TypeStatic, DenseTranslation)

            // 0000010B:  88 0D 00 01 0C 03 00 00     "........"
            // 00000110:  00 00 F7 0C 00 00 F8 0C     "........"
            0x88, 0x0D, 0x00, 0x01, 0x0C, 0x03, 0x00, 0x00, 0x00, 0x00, 0xF7, 0x0C, 0x00, 0x00, 0xF8, 0x0C,
            //     118:               WordIO (ResourceProducer, MinFixed, MaxFixed, PosDecode, EntireRange,
            //     119:                   0x0000,             // Granularity
            //     120:                   0x0D00,             // Range Minimum
            //     121:                   0xFFFF,             // Range Maximum
            //     122:                   0x0000,             // Translation Offset
            //     123:                   0xF300,             // Length
            //     124:                   ,, , TypeStatic, DenseTranslation)

            // 0000011B:  88 0D 00 01 0C 03 00 00     "........"
            // 00000123:  00 0D FF FF 00 00 00 F3     "........"
            0x88, 0x0D, 0x00, 0x01, 0x0C, 0x03, 0x00, 0x00, 0x00, 0x0D, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0xF3,
            //     125:               DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, Cacheable, ReadWrite,
            //     126:                   0x00000000,         // Granularity
            //     127:                   0x000A0000,         // Range Minimum
            //     128:                   0x000BFFFF,         // Range Maximum
            //     129:                   0x00000000,         // Translation Offset
            //     130:                   0x00020000,         // Length
            //     131:                   ,, , AddressRangeMemory, TypeStatic)

            // 0000012B:  87 17 00 00 0C 03 00 00     "........"
            // 00000133:  00 00 00 00 0A 00 FF FF     "........"
            // 0000013B:  0B 00 00 00 00 00 00 00     "........"
            // 00000143:  02 00 ..................    ".."
            0x87, 0x17, 0x00, 0x00, 0x0C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x00, 0xFF, 0xFF, 0x0B,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            //     132:               DWordMemory (ResourceProducer, PosDecode, MinFixed, MaxFixed, NonCacheable, ReadWrite,
            //     133:                   0x00000000,         // Granularity
            //     134:                   0xE0000000,         // Range Minimum
            //     135:                   0xFEBFFFFF,         // Range Maximum
            //     136:                   0x00000000,         // Translation Offset
            //     137:                   0x1EC00000,         // Length
            //     138:                   ,, _Y00, AddressRangeMemory, TypeStatic)

            // 00000145:  87 17 00 00 0C 01 00 00     "........"
            // 0000014D:  00 00 00 00 00 E0 FF FF     "........"
            // 00000155:  BF FE 00 00 00 00 00 00     "........"
            // 0000015D:  C0 1E ..................    ".."
            0x87, 0x17, 0x00, 0x00, 0x0C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE0, 0xFF, 0xFF, 0xBF,
            0xFE, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0, 0x1E,
            // 0000015F:  79 00 ..................    "y."
            0x79, 0x00,
        ]
        .to_vec();

        let value = Object::Buffer(bytes).wrap();
        let resources = resource_descriptor_list(value).unwrap();

        assert_eq!(
            resources,
            Vec::from([
                Resource::AddressSpace(AddressSpaceDescriptor {
                    resource_type: AddressSpaceResourceType::BusNumberRange,
                    is_maximum_address_fixed: true,
                    is_minimum_address_fixed: true,
                    decode_type: AddressSpaceDecodeType::Additive,
                    granularity: 0,
                    address_range: (0x00, 0xFF),
                    translation_offset: 0,
                    length: 0x100
                }),
                Resource::IOPort(IOPortDescriptor {
                    decodes_full_address: true,
                    memory_range: (0xCF8, 0xCF8),
                    base_alignment: 1,
                    range_length: 8
                }),
                Resource::AddressSpace(AddressSpaceDescriptor {
                    resource_type: AddressSpaceResourceType::IORange,
                    is_maximum_address_fixed: true,
                    is_minimum_address_fixed: true,
                    decode_type: AddressSpaceDecodeType::Additive,
                    granularity: 0,
                    address_range: (0x0000, 0x0CF7),
                    translation_offset: 0,
                    length: 0xCF8
                }),
                Resource::AddressSpace(AddressSpaceDescriptor {
                    resource_type: AddressSpaceResourceType::IORange,
                    is_maximum_address_fixed: true,
                    is_minimum_address_fixed: true,
                    decode_type: AddressSpaceDecodeType::Additive,
                    granularity: 0,
                    address_range: (0x0D00, 0xFFFF),
                    translation_offset: 0,
                    length: 0xF300
                }),
                Resource::AddressSpace(AddressSpaceDescriptor {
                    resource_type: AddressSpaceResourceType::MemoryRange,
                    is_maximum_address_fixed: true,
                    is_minimum_address_fixed: true,
                    decode_type: AddressSpaceDecodeType::Additive,
                    granularity: 0,
                    address_range: (0xA0000, 0xBFFFF),
                    translation_offset: 0,
                    length: 0x20000
                }),
                Resource::AddressSpace(AddressSpaceDescriptor {
                    resource_type: AddressSpaceResourceType::MemoryRange,
                    is_maximum_address_fixed: true,
                    is_minimum_address_fixed: true,
                    decode_type: AddressSpaceDecodeType::Additive,
                    granularity: 0,
                    address_range: (0xE0000000, 0xFEBFFFFF),
                    translation_offset: 0,
                    length: 0x1EC00000
                }),
            ])
        );
    }

    #[test]
    fn test_fdc_crs() {
        let bytes: Vec<u8> = [
            //         365:                   IO (Decode16,
            //         366:                       0x03F2,             // Range Minimum
            //         367:                       0x03F2,             // Range Maximum
            //         368:                       0x00,               // Alignment
            //         369:                       0x04,               // Length
            //         370:                       )

            //    0000047C:  47 01 F2 03 F2 03 00 04     "G......."
            0x47, 0x01, 0xF2, 0x03, 0xF2, 0x03, 0x00, 0x04,
            //         371:                   IO (Decode16,
            //         372:                       0x03F7,             // Range Minimum
            //         373:                       0x03F7,             // Range Maximum
            //         374:                       0x00,               // Alignment
            //         375:                       0x01,               // Length
            //         376:                       )

            //    00000484:  47 01 F7 03 F7 03 00 01     "G......."
            0x47, 0x01, 0xF7, 0x03, 0xF7, 0x03, 0x00, 0x01,
            //         377:                   IRQNoFlags ()
            //         378:                       {6}

            //    0000048C:  22 40 00 ...............    ""@."
            0x22, 0x40, 0x00,
            //         379:                   DMA (Compatibility, NotBusMaster, Transfer8, )
            //         380:                       {2}

            //    0000048F:  2A 04 00 ...............    "*.."
            0x2A, 0x04, 0x00, //    00000492:  79 00 ..................    "y."
            0x79, 0x00,
        ]
        .to_vec();

        let value = Object::Buffer(bytes).wrap();
        let resources = resource_descriptor_list(value).unwrap();

        assert_eq!(
            resources,
            Vec::from([
                Resource::IOPort(IOPortDescriptor {
                    decodes_full_address: true,
                    memory_range: (0x03F2, 0x03F2),
                    base_alignment: 0,
                    range_length: 4
                }),
                Resource::IOPort(IOPortDescriptor {
                    decodes_full_address: true,
                    memory_range: (0x03F7, 0x03F7),
                    base_alignment: 0,
                    range_length: 1
                }),
                Resource::Irq(IrqDescriptor {
                    is_consumer: false,
                    trigger: InterruptTrigger::Edge,
                    polarity: InterruptPolarity::ActiveHigh,
                    is_shared: false,
                    is_wake_capable: false,
                    irqs: Vec::from([6]),
                    source: None,
                }),
                Resource::Dma(DMADescriptor {
                    channel_mask: 1 << 2,
                    supported_speeds: DMASupportedSpeed::CompatibilityMode,
                    is_bus_master: false,
                    transfer_type_preference: DMATransferTypePreference::_8BitOnly
                })
            ])
        );
    }

    #[test]
    fn test_small_irq_mask_is_decoded_as_irq_number() {
        // Mask bit 10 is the IRQ number; the raw mask value is 0x0400.
        let descriptor = irq(&[0x22, 0x00, 0x04, 0x79, 0x00]);
        assert_eq!(descriptor.irqs, Vec::from([10]));
        assert_eq!(descriptor.trigger, InterruptTrigger::Edge);
        assert_eq!(descriptor.polarity, InterruptPolarity::ActiveHigh);
    }

    #[test]
    fn test_small_irq_level_low_flags() {
        // Level-triggered, active-low, shared, wake-capable.
        let descriptor = irq(&[0x23, 0x00, 0x08, 0x38, 0x79, 0x00]);
        assert_eq!(descriptor.irqs, Vec::from([11]));
        assert_eq!(descriptor.trigger, InterruptTrigger::Level);
        assert_eq!(descriptor.polarity, InterruptPolarity::ActiveLow);
        assert!(descriptor.is_shared);
        assert!(descriptor.is_wake_capable);
    }

    #[test]
    fn test_zero_irq_mask_is_malformed() {
        let mut descriptors = resource_descriptors(&[0x22, 0x00, 0x00, 0x79, 0x00]);
        assert_eq!(descriptors.next().unwrap().unwrap().decode(), Err(AmlError::InvalidResourceDescriptor));
    }

    #[test]
    fn test_extended_irq_multiple_vectors() {
        let descriptor = irq(&[
            0x89, 0x0a, 0x00, 0x1d, 0x02, 0x10, 0x00, 0x00, 0x00, 0x11, 0x00, 0x00, 0x00, 0x79, 0x00,
        ]);
        assert_eq!(descriptor.irqs, Vec::from([16, 17]));
        assert!(descriptor.is_consumer);
        assert_eq!(descriptor.trigger, InterruptTrigger::Level);
        assert_eq!(descriptor.polarity, InterruptPolarity::ActiveLow);
        assert!(descriptor.is_shared);
        assert!(descriptor.is_wake_capable);
    }

    #[test]
    fn test_extended_irq_zero_count_is_malformed() {
        let mut descriptors = resource_descriptors(&[0x89, 0x03, 0x00, 0x00, 0x00, 0x79, 0x00]);
        assert_eq!(descriptors.next().unwrap().unwrap().decode(), Err(AmlError::InvalidResourceDescriptor));
    }

    #[test]
    fn test_extended_irq_resource_source_suffix_is_retained() {
        // One vector, then ResourceSourceIndex = 1 and "LNK0\0".
        let descriptor = irq(&[
            0x89, 0x0c, 0x00, 0x0d, 0x01, 0x14, 0x00, 0x00, 0x00, 0x01, b'L', b'N', b'K', b'0',
            0x00, 0x79, 0x00,
        ]);
        assert_eq!(descriptor.irqs, Vec::from([20]));
        assert_eq!(
            descriptor.source,
            Some(ResourceSource { index: 1, name: String::from("LNK0") })
        );
    }

    #[test]
    fn test_extended_irq_malformed_resource_source_suffix() {
        // A single suffix byte cannot hold the index and a terminator.
        let mut descriptors = resource_descriptors(&[
            0x89, 0x07, 0x00, 0x0d, 0x01, 0x14, 0x00, 0x00, 0x00, 0x00, 0x79, 0x00,
        ]);
        assert_eq!(
            descriptors.next().unwrap().unwrap().decode(),
            Err(AmlError::InvalidResourceDescriptor)
        );

        // The string must be null-terminated.
        let mut descriptors = resource_descriptors(&[
            0x89, 0x08, 0x00, 0x0d, 0x01, 0x14, 0x00, 0x00, 0x00, 0x00, b'L', 0x79, 0x00,
        ]);
        assert_eq!(
            descriptors.next().unwrap().unwrap().decode(),
            Err(AmlError::InvalidResourceDescriptor)
        );

        // Bytes after the terminator are not part of the suffix.
        let mut descriptors = resource_descriptors(&[
            0x89, 0x0a, 0x00, 0x0d, 0x01, 0x14, 0x00, 0x00, 0x00, 0x00, b'L', 0x00, b'X', 0x79,
            0x00,
        ]);
        assert_eq!(
            descriptors.next().unwrap().unwrap().decode(),
            Err(AmlError::InvalidResourceDescriptor)
        );
    }

    #[test]
    fn test_truncated_end_tag_is_an_error() {
        let mut descriptors = resource_descriptors(&[0x22, 0x02, 0x00, 0x79]);
        assert!(descriptors.next().unwrap().is_ok());
        assert_eq!(
            descriptors.next(),
            Some(Err(AmlError::InvalidResourceDescriptor))
        );
        assert!(descriptors.next().is_none());
        assert!(!descriptors.end_tag_seen());
    }

    #[test]
    fn test_word_address_space_widths() {
        // WordBusNumber, granularity 0x0003, minimum 0x0001, maximum 0x00ff,
        // translation 0x0002, length 0x0100.
        let bytes = [
            0x88, 0x0d, 0x00, // Word Address Space, 13-byte body
            0x02, 0x0c, 0x00, // bus-number type, general/type flags
            0x03, 0x00, // granularity
            0x01, 0x00, // minimum
            0xff, 0x00, // maximum
            0x02, 0x00, // translation
            0x00, 0x01, // length
            0x79, 0x00,
        ];
        assert_eq!(
            decode(&bytes),
            Vec::from([Resource::AddressSpace(AddressSpaceDescriptor {
                resource_type: AddressSpaceResourceType::BusNumberRange,
                is_maximum_address_fixed: true,
                is_minimum_address_fixed: true,
                decode_type: AddressSpaceDecodeType::Additive,
                granularity: 0x0003,
                address_range: (0x0001, 0x00ff),
                translation_offset: 0x0002,
                length: 0x0100,
            })])
        );
    }

    #[test]
    fn test_dword_address_space_widths_and_offsets() {
        // DWordMemory with every address field distinct, so a width or offset
        // mistake cannot pass by coincidence.
        let bytes = [
            0x87, 0x17, 0x00, // DWord Address Space, 23-byte body
            0x00, 0x0c, 0x00, // memory-range type, flags
            0xff, 0xff, 0x0f, 0x00, // granularity = 0x000f_ffff
            0x45, 0x23, 0x01, 0x00, // minimum = 0x0001_2345
            0x56, 0x34, 0x02, 0x00, // maximum = 0x0002_3456
            0x00, 0x10, 0x00, 0x00, // translation = 0x0000_1000
            0x11, 0x11, 0x01, 0x00, // length = 0x0001_1111
            0x79, 0x00,
        ];
        assert_eq!(
            decode(&bytes),
            Vec::from([Resource::AddressSpace(AddressSpaceDescriptor {
                resource_type: AddressSpaceResourceType::MemoryRange,
                is_maximum_address_fixed: true,
                is_minimum_address_fixed: true,
                decode_type: AddressSpaceDecodeType::Additive,
                granularity: 0x000f_ffff,
                address_range: (0x0001_2345, 0x0002_3456),
                translation_offset: 0x0000_1000,
                length: 0x0001_1111,
            })])
        );
    }

    #[test]
    fn test_truncated_large_item_is_an_error() {
        let mut descriptors = resource_descriptors(&[0x88, 0x0d, 0x00, 0x02]);
        assert_eq!(descriptors.next().unwrap(), Err(AmlError::InvalidResourceDescriptor));
        assert!(descriptors.next().is_none());
    }

    #[test]
    fn test_unknown_item_is_skipped_but_framing_is_checked() {
        // Small vendor-defined item (type 0x0e, length 2) followed by an IRQ.
        let resources = decode(&[0x72, 0xaa, 0xbb, 0x22, 0x02, 0x00, 0x79, 0x00]);
        assert_eq!(resources.len(), 1);
        assert!(matches!(&resources[0], Resource::Irq(_)));
    }

    #[test]
    fn test_end_tag_is_reported() {
        let mut descriptors = resource_descriptors(&[0x79, 0x00]);
        assert!(descriptors.next().is_none());
        assert!(descriptors.end_tag_seen());

        let mut descriptors = resource_descriptors(&[0x22, 0x02, 0x00]);
        assert!(descriptors.next().is_some());
        assert!(descriptors.next().is_none());
        assert!(!descriptors.end_tag_seen());
    }
}
