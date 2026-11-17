// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Errors reported while parsing ELF data or constructing its initial stack.

/// A typed failure from ELF parsing or initial-stack construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ElfError {
    /// The ELF header parser rejected the input.
    InvalidHeader(&'static str),
    /// The header buffer is misaligned for the class-specific header part.
    MisalignedHeader,
    /// The ELF class is unsupported.
    InvalidClass,
    /// The program-header entry size does not match the ELF class.
    InvalidProgramHeaderEntrySize,
    /// The program-header table size cannot be represented.
    ProgramHeaderTableSizeOverflow,
    /// The program-header table end cannot be represented.
    ProgramHeaderTableRangeOverflow,
    /// The supplied program-header bytes do not match the declared table size.
    IncompleteProgramHeaderTable,
    /// A program-header entry is shorter than its class-specific layout.
    TruncatedProgramHeaderEntry,
    /// No load segment covers the program-header table offset.
    ProgramHeaderTableUnmapped,
    /// The biased program-header address cannot be represented.
    ProgramHeaderAddressOverflow,
    /// The initial user stack size exceeds the available address range.
    UserStackOverflow,
}

impl core::fmt::Display for ElfError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::InvalidHeader(message) => message,
            Self::MisalignedHeader => "ELF header buffer is misaligned",
            Self::InvalidClass => "Invalid ELF class",
            Self::InvalidProgramHeaderEntrySize => "Invalid program-header entry size",
            Self::ProgramHeaderTableSizeOverflow => "Program-header table size overflows",
            Self::ProgramHeaderTableRangeOverflow => "Program-header table range overflows",
            Self::IncompleteProgramHeaderTable => "Incomplete program-header table",
            Self::TruncatedProgramHeaderEntry => "Truncated program-header entry",
            Self::ProgramHeaderTableUnmapped => "program-header table is not covered by a segment",
            Self::ProgramHeaderAddressOverflow => "program-header address overflows",
            Self::UserStackOverflow => "user stack overflow",
        })
    }
}
