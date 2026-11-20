// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crc::{CRC_32_ISCSI, Crc};

/// Linux ext4 uses the reflected CRC32C polynomial with a caller-supplied raw
/// register value and no final complement. `CRC_32_ISCSI` supplies the
/// polynomial and table; its public `Digest` applies the algorithm's
/// initial/final reflections and XOR, so convert the raw register to and from
/// that representation here.
static CRC32C: Crc<u32> = Crc::<u32>::new(&CRC_32_ISCSI);

/// Updates a Linux-style raw CRC32C register without a final complement.
pub(crate) fn crc32c(crc: u32, bytes: &[u8]) -> u32 {
    let mut digest = CRC32C.digest_with_initial(crc.reverse_bits());
    digest.update(bytes);
    digest.finalize() ^ u32::MAX
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

unittest::path_mod_test!(tests, "../../ut-tests/disk/checksum/tests.rs");
