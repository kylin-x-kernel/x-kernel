//! Allocation-free, bounds-checked MADT entry decoding.

/// MADT table header facts: the legacy Local APIC address and flags.
#[derive(Debug, Clone, Copy)]
pub struct MadtInfo {
    /// Physical address of the local APIC register block.
    pub local_apic_address: u32,
    /// Raw MADT flags (e.g. PC-AT compatibility bit).
    pub flags: u32,
}

/// One processor entry from the MADT (type 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalApicEntry {
    /// Processor UID as listed by the firmware (often the ACPI UID).
    pub processor_uid: u8,
    /// APIC id used to address the processor's local APIC.
    pub apic_id: u8,
    /// Raw MADT flags; bit 0 is the enabled bit (see [`LocalApicEntry::enabled`]).
    pub flags: u32,
}

impl LocalApicEntry {
    /// Returns whether the firmware marked this processor usable.
    pub const fn enabled(self) -> bool {
        (self.flags & 0x1) != 0
    }
}

/// One I/O APIC entry from the MADT (type 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoApicEntry {
    /// I/O APIC id as listed by the firmware.
    pub id: u8,
    /// Physical address of the I/O APIC register block.
    pub address: u32,
    /// First global system interrupt number routed through this APIC.
    pub global_system_interrupt_base: u32,
}

/// One interrupt source override from the MADT (type 2).
///
/// The flags are kept raw: trigger and polarity policy belongs to the
/// architecture integration layer, not to ACPI table parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptSourceOverride {
    /// Bus the source interrupt belongs to (0 is ISA).
    pub bus: u8,
    /// Bus-relative source interrupt number.
    pub source: u8,
    /// Global system interrupt it is routed to.
    pub gsi: u32,
    /// Raw MPS INTI flags (polarity in bits 0:1, trigger mode in bits 2:3).
    pub flags: u16,
}

/// A parsed MADT entry, narrowed to the kinds the kernel consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtEntry {
    /// Processor entry (MADT type 0).
    LocalApic(LocalApicEntry),
    /// I/O APIC entry (MADT type 1).
    IoApic(IoApicEntry),
    /// Interrupt source override (MADT type 2).
    InterruptSourceOverride(InterruptSourceOverride),
}

/// Why a MADT entry could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtParseErrorKind {
    /// The entry header or declared body runs past the entry region.
    TruncatedEntry,
    /// The entry declares a length smaller than the two-byte entry header.
    InvalidLength,
    /// A known entry is shorter than its fixed structure.
    ShortEntry,
}

/// A MADT framing error and the table-relative offset it was detected at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadtParseError {
    /// Offset from the start of the MADT table.
    pub offset: usize,
    /// What made the entry unparseable.
    pub kind: MadtParseErrorKind,
}

/// Iterator over the variable-length entries of a validated MADT.
///
/// Entry kinds the kernel does not model are skipped. A malformed entry
/// yields one [`MadtParseError`] and then ends the iteration; it is never
/// treated as a panic or silently dropped.
#[derive(Clone, Copy)]
pub struct MadtEntryIter<'a> {
    entries: &'a [u8],
    /// Table-relative offset of the first byte of `entries`.
    base_offset: usize,
    offset: usize,
    done: bool,
}

impl<'a> MadtEntryIter<'a> {
    /// Iterates over a raw MADT entry region.
    ///
    /// `base_offset` is the table-relative offset of `entries[0]` and is
    /// added to offsets reported in [`MadtParseError`].
    pub const fn new(entries: &'a [u8], base_offset: usize) -> Self {
        Self { entries, base_offset, offset: 0, done: false }
    }
}

/// Decodes the fixed MADT body and returns its fallible entry stream.
pub fn decode(table: super::Table<'_>) -> Result<(MadtInfo, MadtEntryIter<'_>), super::Error> {
    table.require_signature(*b"APIC")?;
    let body = table.body();
    let fixed = body.get(..MADT_HEADER_SIZE).ok_or(super::Error::Truncated)?;
    let info = MadtInfo {
        local_apic_address: u32::from_le_bytes([fixed[0], fixed[1], fixed[2], fixed[3]]),
        flags: u32::from_le_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]),
    };
    Ok((info, MadtEntryIter::new(&body[MADT_HEADER_SIZE..], super::HEADER_SIZE + MADT_HEADER_SIZE)))
}

impl Iterator for MadtEntryIter<'_> {
    type Item = Result<MadtEntry, MadtParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.done {
            let remaining = self.entries.get(self.offset..)?;
            if remaining.is_empty() {
                self.done = true;
                return None;
            }
            let offset = self.base_offset + self.offset;
            if remaining.len() < ENTRY_HEADER_SIZE {
                self.done = true;
                return Some(Err(MadtParseError { offset, kind: MadtParseErrorKind::TruncatedEntry }));
            }

            let entry_len = remaining[1] as usize;
            if entry_len < ENTRY_HEADER_SIZE {
                self.done = true;
                return Some(Err(MadtParseError { offset, kind: MadtParseErrorKind::InvalidLength }));
            }
            let Some(entry) = remaining.get(..entry_len) else {
                self.done = true;
                return Some(Err(MadtParseError { offset, kind: MadtParseErrorKind::TruncatedEntry }));
            };
            self.offset += entry_len;

            let parsed = match remaining[0] {
                MADT_TYPE_LOCAL_APIC => parse_local_apic(entry),
                MADT_TYPE_IO_APIC => parse_io_apic(entry),
                MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE => parse_interrupt_source_override(entry),
                // Entries the kernel does not model are still framed, so skip
                // them without interpreting their payload.
                _ => continue,
            };
            match parsed {
                Ok(parsed) => return Some(Ok(parsed)),
                Err(kind) => {
                    // A malformed known entry invalidates the rest of the
                    // stream: its length no longer proves where the next
                    // entry starts.
                    self.done = true;
                    return Some(Err(MadtParseError { offset, kind }));
                }
            }
        }

        None
    }
}

fn parse_local_apic(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..LOCAL_APIC_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::LocalApic(LocalApicEntry {
        processor_uid: body[2],
        apic_id: body[3],
        flags: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
    }))
}

fn parse_io_apic(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..IO_APIC_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::IoApic(IoApicEntry {
        id: body[2],
        address: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        global_system_interrupt_base: u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
    }))
}

fn parse_interrupt_source_override(entry: &[u8]) -> Result<MadtEntry, MadtParseErrorKind> {
    let Some(body) = entry.get(..INTERRUPT_SOURCE_OVERRIDE_ENTRY_SIZE) else {
        return Err(MadtParseErrorKind::ShortEntry);
    };
    Ok(MadtEntry::InterruptSourceOverride(InterruptSourceOverride {
        bus: body[2],
        source: body[3],
        gsi: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        flags: u16::from_le_bytes([body[8], body[9]]),
    }))
}

const MADT_HEADER_SIZE: usize = 8;
const ENTRY_HEADER_SIZE: usize = 2;
const LOCAL_APIC_ENTRY_SIZE: usize = 8;
const IO_APIC_ENTRY_SIZE: usize = 12;
const INTERRUPT_SOURCE_OVERRIDE_ENTRY_SIZE: usize = 10;

const MADT_TYPE_LOCAL_APIC: u8 = 0;
const MADT_TYPE_IO_APIC: u8 = 1;
const MADT_TYPE_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;
