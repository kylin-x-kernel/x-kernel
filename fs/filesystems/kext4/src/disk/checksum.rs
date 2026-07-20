// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

const CRC32C_POLYNOMIAL: u32 = 0x82f6_3b78;

/// Updates a Linux-style CRC32C value without applying a final complement.
pub(crate) fn crc32c(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (CRC32C_POLYNOMIAL & mask);
        }
    }
    crc
}

pub(crate) fn group_descriptor_checksum(
    descriptor_bytes: &[u8],
    group: u32,
    checksum_seed: u32,
) -> Option<u16> {
    let before_checksum = descriptor_bytes.get(..30)?;
    let after_checksum = descriptor_bytes.get(32..)?;

    let mut checksum = crc32c(checksum_seed, &group.to_le_bytes());
    checksum = crc32c(checksum, before_checksum);
    checksum = crc32c(checksum, &[0, 0]);
    checksum = crc32c(checksum, after_checksum);
    Some(checksum as u16)
}

pub(crate) fn bitmap_checksum(bitmap: &[u8], checksum_seed: u32) -> u32 {
    crc32c(checksum_seed, bitmap)
}

#[cfg(test)]
mod tests {
    use super::{bitmap_checksum, crc32c};

    #[test]
    fn linux_crc32c_golden_vector() {
        assert_eq!(crc32c(u32::MAX, b"123456789"), 0x1cf9_6d7c);
    }

    #[test]
    fn bitmap_checksum_matches_linux_seeded_bitmap_crc() {
        let seed = 0x1122_3344;
        let bitmap = [0b1010_0101, 0x5a, 0xff, 0x00];

        assert_eq!(bitmap_checksum(&bitmap, seed), crc32c(seed, &bitmap));
    }
}
