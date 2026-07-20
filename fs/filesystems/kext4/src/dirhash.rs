// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Error, Ext4Result, UnsupportedKind};

pub(crate) const DX_HASH_LEGACY: u8 = 0;
pub(crate) const DX_HASH_HALF_MD4: u8 = 1;
pub(crate) const DX_HASH_TEA: u8 = 2;
pub(crate) const DX_HASH_LEGACY_UNSIGNED: u8 = 3;
pub(crate) const DX_HASH_HALF_MD4_UNSIGNED: u8 = 4;
pub(crate) const DX_HASH_TEA_UNSIGNED: u8 = 5;

const DEFAULT_SEED: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
const TEA_DELTA: u32 = 0x9e37_79b9;
const HTREE_EOF_32BIT: u32 = (1u32 << 31) - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryHash {
    major: u32,
    minor: u32,
}

impl DirectoryHash {
    pub(crate) const fn major(self) -> u32 {
        self.major
    }
}

pub(crate) fn directory_hash(
    name: &[u8],
    hash_version: u8,
    seed: [u32; 4],
) -> Ext4Result<DirectoryHash> {
    let mut state = if seed.iter().any(|word| *word != 0) {
        seed
    } else {
        DEFAULT_SEED
    };
    let (mut major, minor) = match hash_version {
        DX_HASH_LEGACY => (legacy_hash(name, false), 0),
        DX_HASH_LEGACY_UNSIGNED => (legacy_hash(name, true), 0),
        DX_HASH_HALF_MD4 => half_md4_hash(name, false, &mut state),
        DX_HASH_HALF_MD4_UNSIGNED => half_md4_hash(name, true, &mut state),
        DX_HASH_TEA => tea_hash(name, false, &mut state),
        DX_HASH_TEA_UNSIGNED => tea_hash(name, true, &mut state),
        _ => return Err(Ext4Error::Unsupported(UnsupportedKind::IndexedDirectory)),
    };
    major &= !1;
    if major == (HTREE_EOF_32BIT << 1) {
        major = (HTREE_EOF_32BIT - 1) << 1;
    }
    Ok(DirectoryHash { major, minor })
}

fn legacy_hash(name: &[u8], unsigned: bool) -> u32 {
    let mut hash0 = 0x12a3_fe2d_u32;
    let mut hash1 = 0x37ab_e8f9_u32;
    for byte in name {
        let value = if unsigned {
            i32::from(*byte)
        } else {
            i32::from(*byte as i8)
        };
        let product = value.wrapping_mul(7_152_373) as u32;
        let mut hash = hash1.wrapping_add(hash0 ^ product);
        if hash & 0x8000_0000 != 0 {
            hash = hash.wrapping_sub(0x7fff_ffff);
        }
        hash1 = hash0;
        hash0 = hash;
    }
    hash0 << 1
}

fn half_md4_hash(name: &[u8], unsigned: bool, state: &mut [u32; 4]) -> (u32, u32) {
    let mut offset = 0usize;
    while offset < name.len() {
        let input = str_to_hashbuf(&name[offset..], 8, unsigned);
        half_md4_transform(state, input);
        offset = offset.saturating_add(32);
    }
    (state[1], state[2])
}

fn tea_hash(name: &[u8], unsigned: bool, state: &mut [u32; 4]) -> (u32, u32) {
    let mut offset = 0usize;
    while offset < name.len() {
        let input = str_to_hashbuf(&name[offset..], 4, unsigned);
        tea_transform(state, [input[0], input[1], input[2], input[3]]);
        offset = offset.saturating_add(16);
    }
    (state[0], state[1])
}

fn str_to_hashbuf(name: &[u8], words: usize, unsigned: bool) -> [u32; 8] {
    let len = name.len().min(words * 4);
    let mut output = [0u32; 8];
    let mut pad = len as u32 | ((len as u32) << 8);
    pad |= pad << 16;

    let mut offset = 0usize;
    let mut index = 0usize;
    while len.saturating_sub(offset) >= 4 {
        output[index] = if unsigned {
            u32::from_be_bytes([
                name[offset],
                name[offset + 1],
                name[offset + 2],
                name[offset + 3],
            ])
        } else {
            signed_byte_word(name[offset], 24)
                .wrapping_add(signed_byte_word(name[offset + 1], 16))
                .wrapping_add(signed_byte_word(name[offset + 2], 8))
                .wrapping_add(signed_byte_word(name[offset + 3], 0))
        };
        offset += 4;
        index += 1;
    }

    if index < words {
        let mut value = pad;
        while offset < len {
            let byte = if unsigned {
                u32::from(name[offset])
            } else {
                i32::from(name[offset] as i8) as u32
            };
            value = byte.wrapping_add(value << 8);
            offset += 1;
        }
        output[index] = value;
        index += 1;
    }
    while index < words {
        output[index] = pad;
        index += 1;
    }
    output
}

fn signed_byte_word(byte: u8, shift: u32) -> u32 {
    (i32::from(byte as i8) as u32) << shift
}

fn tea_transform(state: &mut [u32; 4], input: [u32; 4]) {
    let mut sum = 0u32;
    let mut left = state[0];
    let mut right = state[1];
    for _ in 0..16 {
        sum = sum.wrapping_add(TEA_DELTA);
        left = left.wrapping_add(
            ((right << 4).wrapping_add(input[0]))
                ^ right.wrapping_add(sum)
                ^ ((right >> 5).wrapping_add(input[1])),
        );
        right = right.wrapping_add(
            ((left << 4).wrapping_add(input[2]))
                ^ left.wrapping_add(sum)
                ^ ((left >> 5).wrapping_add(input[3])),
        );
    }
    state[0] = state[0].wrapping_add(left);
    state[1] = state[1].wrapping_add(right);
}

fn half_md4_transform(state: &mut [u32; 4], input: [u32; 8]) {
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];

    round1(&mut a, b, c, d, input[0], 3);
    round1(&mut d, a, b, c, input[1], 7);
    round1(&mut c, d, a, b, input[2], 11);
    round1(&mut b, c, d, a, input[3], 19);
    round1(&mut a, b, c, d, input[4], 3);
    round1(&mut d, a, b, c, input[5], 7);
    round1(&mut c, d, a, b, input[6], 11);
    round1(&mut b, c, d, a, input[7], 19);

    round2(&mut a, b, c, d, input[1], 3);
    round2(&mut d, a, b, c, input[3], 5);
    round2(&mut c, d, a, b, input[5], 9);
    round2(&mut b, c, d, a, input[7], 13);
    round2(&mut a, b, c, d, input[0], 3);
    round2(&mut d, a, b, c, input[2], 5);
    round2(&mut c, d, a, b, input[4], 9);
    round2(&mut b, c, d, a, input[6], 13);

    round3(&mut a, b, c, d, input[3], 3);
    round3(&mut d, a, b, c, input[7], 9);
    round3(&mut c, d, a, b, input[2], 11);
    round3(&mut b, c, d, a, input[6], 15);
    round3(&mut a, b, c, d, input[1], 3);
    round3(&mut d, a, b, c, input[5], 9);
    round3(&mut c, d, a, b, input[0], 11);
    round3(&mut b, c, d, a, input[4], 15);

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

fn round1(target: &mut u32, b: u32, c: u32, d: u32, word: u32, shift: u32) {
    *target = target
        .wrapping_add(d ^ (b & (c ^ d)))
        .wrapping_add(word)
        .rotate_left(shift);
}

fn round2(target: &mut u32, b: u32, c: u32, d: u32, word: u32, shift: u32) {
    *target = target
        .wrapping_add((b & c).wrapping_add((b ^ c) & d))
        .wrapping_add(word)
        .wrapping_add(0x5a82_7999)
        .rotate_left(shift);
}

fn round3(target: &mut u32, b: u32, c: u32, d: u32, word: u32, shift: u32) {
    *target = target
        .wrapping_add(b ^ c ^ d)
        .wrapping_add(word)
        .wrapping_add(0x6ed9_eba1)
        .rotate_left(shift);
}
