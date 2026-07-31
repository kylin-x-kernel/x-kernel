// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Metadata ABI shared by X-Kernel image tooling and early boot code.
//!
//! XKMake encodes build provenance into [`BUILD_INFO_SECTION_NAME`] after
//! Cargo links the kernel. The kernel feature additionally exposes the
//! linker-delimited metadata through [`embedded_build_info`] and
//! [`embedded_build_id`].

#![no_std]

use core::{fmt, str};

/// ELF section containing the X-Kernel build-information note.
pub const BUILD_INFO_SECTION_NAME: &str = ".note.xkernel.build-info";

/// ELF section containing the GNU-compatible build ID note.
pub const BUILD_ID_SECTION_NAME: &str = ".note.gnu.build-id";

/// Number of descriptor bytes reserved for encoded build information.
pub const BUILD_INFO_DESCRIPTOR_SIZE: usize = 1024;

/// Number of bytes in the SHA-256 GNU build ID descriptor.
pub const BUILD_ID_SIZE: usize = 32;

/// Owner name stored in the X-Kernel ELF note, including its terminator.
pub const BUILD_INFO_NOTE_OWNER: &[u8; 8] = b"XKERNEL\0";

/// X-Kernel ELF note type for format version 1 build information.
pub const BUILD_INFO_NOTE_TYPE: u32 = 0x584b_0001;

const MAGIC: &[u8; 8] = b"XKBUILD\0";
const FORMAT_VERSION: u16 = 1;
const HEADER_SIZE: usize = 20;
const CRC32_POLYNOMIAL: u32 = 0xedb8_8320;

/// Error returned when image metadata cannot be encoded or validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    /// The reserved descriptor is smaller than the format header.
    SectionTooSmall,
    /// The payload does not fit in the reserved descriptor.
    PayloadTooLarge,
    /// The descriptor does not contain the X-Kernel build-info magic.
    InvalidMagic,
    /// The encoded format version is unsupported.
    UnsupportedVersion,
    /// The encoded header length is invalid.
    InvalidHeaderSize,
    /// The encoded payload range extends beyond the descriptor.
    InvalidPayloadSize,
    /// The payload checksum does not match its encoded value.
    ChecksumMismatch,
    /// The payload is not valid UTF-8.
    InvalidUtf8,
    /// Linker symbols describe an invalid or empty memory range.
    InvalidLinkerRange,
    /// The embedded build ID has an unexpected length.
    InvalidBuildIdSize,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SectionTooSmall => "build-info descriptor is smaller than its header",
            Self::PayloadTooLarge => "build-info payload does not fit in the reserved descriptor",
            Self::InvalidMagic => "build-info magic is invalid",
            Self::UnsupportedVersion => "build-info format version is unsupported",
            Self::InvalidHeaderSize => "build-info header size is invalid",
            Self::InvalidPayloadSize => "build-info payload size is invalid",
            Self::ChecksumMismatch => "build-info payload checksum does not match",
            Self::InvalidUtf8 => "build-info payload is not valid UTF-8",
            Self::InvalidLinkerRange => "linker metadata range is invalid",
            Self::InvalidBuildIdSize => "embedded build ID size is invalid",
        })
    }
}

/// Encodes `payload` into a linker-reserved build-info note descriptor.
///
/// Existing descriptor contents are cleared so padding remains deterministic.
///
/// # Errors
///
/// Returns [`MetadataError::SectionTooSmall`] when `descriptor` cannot hold
/// the header, or [`MetadataError::PayloadTooLarge`] when the payload does not
/// fit.
pub fn encode_build_info(payload: &str, descriptor: &mut [u8]) -> Result<usize, MetadataError> {
    if descriptor.len() < HEADER_SIZE {
        return Err(MetadataError::SectionTooSmall);
    }
    let payload_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| MetadataError::PayloadTooLarge)?;
    let payload_end = HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(MetadataError::PayloadTooLarge)?;
    if payload_end > descriptor.len() {
        return Err(MetadataError::PayloadTooLarge);
    }

    descriptor.fill(0);
    descriptor[..MAGIC.len()].copy_from_slice(MAGIC);
    descriptor[8..10].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    descriptor[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    descriptor[12..16].copy_from_slice(&payload_len.to_le_bytes());
    descriptor[16..20].copy_from_slice(&crc32(payload.as_bytes()).to_le_bytes());
    descriptor[HEADER_SIZE..payload_end].copy_from_slice(payload.as_bytes());
    Ok(payload_end)
}

/// Validates a build-info descriptor and returns its UTF-8 payload.
///
/// # Errors
///
/// Returns a [`MetadataError`] when the header, bounds, checksum, or UTF-8
/// encoding is invalid.
pub fn decode_build_info(descriptor: &[u8]) -> Result<&str, MetadataError> {
    if descriptor.len() < HEADER_SIZE {
        return Err(MetadataError::SectionTooSmall);
    }
    if &descriptor[..MAGIC.len()] != MAGIC {
        return Err(MetadataError::InvalidMagic);
    }
    if read_u16(&descriptor[8..10]) != FORMAT_VERSION {
        return Err(MetadataError::UnsupportedVersion);
    }
    let header_size = usize::from(read_u16(&descriptor[10..12]));
    if header_size != HEADER_SIZE {
        return Err(MetadataError::InvalidHeaderSize);
    }
    let payload_len = read_u32(&descriptor[12..16]) as usize;
    let payload_end = header_size
        .checked_add(payload_len)
        .ok_or(MetadataError::InvalidPayloadSize)?;
    let payload = descriptor
        .get(header_size..payload_end)
        .ok_or(MetadataError::InvalidPayloadSize)?;
    if crc32(payload) != read_u32(&descriptor[16..20]) {
        return Err(MetadataError::ChecksumMismatch);
    }
    str::from_utf8(payload).map_err(|_| MetadataError::InvalidUtf8)
}

/// Returns the build-information payload embedded in the running kernel.
///
/// # Errors
///
/// Returns a [`MetadataError`] if the linker range or encoded descriptor is
/// invalid.
#[cfg(feature = "kernel")]
pub fn embedded_build_info() -> Result<&'static str, MetadataError> {
    decode_build_info(linker_slice(
        core::ptr::addr_of!(__xkernel_build_info_start).cast(),
        core::ptr::addr_of!(__xkernel_build_info_end).cast(),
    )?)
}

/// Returns the GNU build ID embedded in the running kernel.
///
/// # Errors
///
/// Returns [`MetadataError::InvalidLinkerRange`] for an invalid linker range,
/// or [`MetadataError::InvalidBuildIdSize`] if the descriptor is not SHA-256
/// sized.
#[cfg(feature = "kernel")]
pub fn embedded_build_id() -> Result<&'static [u8], MetadataError> {
    let build_id = linker_slice(
        core::ptr::addr_of!(__xkernel_build_id_start).cast(),
        core::ptr::addr_of!(__xkernel_build_id_end).cast(),
    )?;
    if build_id.len() != BUILD_ID_SIZE {
        return Err(MetadataError::InvalidBuildIdSize);
    }
    Ok(build_id)
}

#[cfg(feature = "kernel")]
unsafe extern "C" {
    safe static __xkernel_build_info_start: [u8; 0];
    safe static __xkernel_build_info_end: [u8; 0];
    safe static __xkernel_build_id_start: [u8; 0];
    safe static __xkernel_build_id_end: [u8; 0];
}

#[cfg(feature = "kernel")]
fn linker_slice(start: *const u8, end: *const u8) -> Result<&'static [u8], MetadataError> {
    let len = (end as usize)
        .checked_sub(start as usize)
        .filter(|len| *len > 0)
        .ok_or(MetadataError::InvalidLinkerRange)?;
    // SAFETY: The linker script defines both symbols around initialized,
    // read-only bytes in a loaded kernel segment. The checked subtraction
    // proves that `start..start + len` is non-empty and does not wrap.
    Ok(unsafe { core::slice::from_raw_parts(start, len) })
}

fn read_u16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC32_POLYNOMIAL & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_preserves_payload() {
        let mut descriptor = [0xa5; BUILD_INFO_DESCRIPTOR_SIZE];
        let expected = "arch = aarch64\nplatform = kplat-aarch64\n";
        let encoded_size = encode_build_info(expected, &mut descriptor).unwrap();
        assert_eq!(decode_build_info(&descriptor), Ok(expected));
        assert_eq!(encoded_size, HEADER_SIZE + expected.len());
        assert!(descriptor[encoded_size..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn corrupted_payload_is_rejected() {
        let mut descriptor = [0; BUILD_INFO_DESCRIPTOR_SIZE];
        encode_build_info("arch = aarch64\n", &mut descriptor).unwrap();
        descriptor[HEADER_SIZE] ^= 1;
        assert_eq!(
            decode_build_info(&descriptor),
            Err(MetadataError::ChecksumMismatch)
        );
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let mut descriptor = [0; BUILD_INFO_DESCRIPTOR_SIZE];
        let payload = "x".repeat(BUILD_INFO_DESCRIPTOR_SIZE);
        assert_eq!(
            encode_build_info(&payload, &mut descriptor),
            Err(MetadataError::PayloadTooLarge)
        );
    }
}
