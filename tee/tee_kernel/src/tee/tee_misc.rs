// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[cfg(unittest)]
use hex::{decode_to_slice, encode_to_slice};

#[cfg(unittest)]
pub const fn tee_b2hs_hsbuf_size(x: usize) -> usize {
    x.saturating_mul(2).saturating_add(1)
}

/// 将二进制数据 `b` 编码为十六进制字符串（写入 `hs`）
///
/// 返回写入的长度（不包含末尾 0）
#[cfg(unittest)]
pub fn tee_b2hs(b: &[u8], hs: &mut [u8]) -> Result<usize, ()> {
    let expected_len = b.len() * 2;

    if hs.len() < expected_len + 1 {
        return Err(()); // 模拟 TEE_ERROR_SHORT_BUFFER
    }

    encode_to_slice(b, &mut hs[..expected_len]).map_err(|_| ())?;

    hs.iter_mut().take(expected_len).for_each(|b| {
        if b'a' <= *b && *b <= b'z' {
            *b = *b - b'a' + b'A';
        }
    });

    hs[expected_len] = 0; // 结尾补 0，用于 C 兼容
    Ok(expected_len)
}

/// 将十六进制字符串 `hs` 解码为二进制（写入 `b`）
///
/// 返回写入的字节数
#[cfg(unittest)]
pub fn tee_hs2b(hs: &[u8], b: &mut [u8]) -> Result<usize, ()> {
    let hslen = hs.len();
    if !hslen.is_multiple_of(2) {
        return Err(()); // 长度必须是偶数
    }

    let expected_len = hslen / 2;
    if b.len() < expected_len {
        return Err(());
    }

    decode_to_slice(hs, &mut b[..expected_len]).map_err(|_| ())?;
    Ok(expected_len)
}

#[unittest::mod_test]
pub mod tests_tee_misc {
    use unittest::{assert, assert_eq};

    use super::*;

    #[unittest::def_test]
    fn test_b2hs_empty_input() {
        let b = &[];
        let mut hs = [0u8; 1];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
        assert_eq!(hs[0], 0);
    }

    #[unittest::def_test]
    fn test_b2hs_single_byte() {
        let b = &[0xAB];
        let mut hs = [0u8; 3];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
        assert_eq!(str::from_utf8(&hs[..2]).unwrap(), "AB");
        assert_eq!(hs[2], 0);
    }

    #[unittest::def_test]
    fn test_b2hs_multiple_bytes() {
        let b = &[0x12, 0x34, 0xCD, 0xEF];
        let mut hs = [0u8; 9];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 8);
        assert_eq!(str::from_utf8(&hs[..8]).unwrap(), "1234CDEF");
        assert_eq!(hs[8], 0);
    }

    #[unittest::def_test]
    fn test_b2hs_short_buffer() {
        let b = &[0x12, 0x34];
        let mut hs = [0u8; 4];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_err());
    }

    #[unittest::def_test]
    fn test_b2hs_exact_buffer_size() {
        let b = &[0xAA];
        let mut hs = [0u8; 3];
        let result = tee_b2hs(b, &mut hs);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 2);
        assert_eq!(str::from_utf8(&hs[..2]).unwrap(), "AA");
        assert_eq!(hs[2], 0);
    }

    #[unittest::def_test]
    fn test_hs2b_empty_input() {
        let hs = &[];
        let mut b = [0u8; 0];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[unittest::def_test]
    fn test_hs2b_single_byte_hex() {
        let hs = b"AB";
        let mut b = [0u8; 1];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 1);
        assert_eq!(b[0], 0xAB);
    }

    #[unittest::def_test]
    fn test_hs2b_multiple_bytes_hex() {
        let hs = b"1234cdef";
        let mut b = [0u8; 4];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 4);
        assert_eq!(b, [0x12, 0x34, 0xCD, 0xEF]);
    }

    #[unittest::def_test]
    fn test_hs2b_odd_length_hex() {
        let hs = b"123";
        let mut b = [0u8; 2];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_err());
    }

    #[unittest::def_test]
    fn test_hs2b_short_buffer() {
        let hs = b"1234";
        let mut b = [0u8; 1];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_err());
    }

    #[unittest::def_test]
    fn test_hs2b_invalid_hex_chars() {
        let hs = b"12gx";
        let mut b = [0u8; 2];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_err());
    }

    #[unittest::def_test]
    fn test_hs2b_uppercase_hex() {
        let hs = b"ABCDEF";
        let mut b = [0u8; 3];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);
        assert_eq!(b, [0xAB, 0xCD, 0xEF]);
    }

    #[unittest::def_test]
    fn test_hs2b_mixed_case_hex() {
        let hs = b"aBcDeF";
        let mut b = [0u8; 3];
        let result = tee_hs2b(hs, &mut b);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 3);
        assert_eq!(b, [0xAB, 0xCD, 0xEF]);
    }
}
