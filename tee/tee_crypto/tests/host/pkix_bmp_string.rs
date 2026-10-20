// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use der::{Tag, asn1::Any};

use super::{Vec, ava_values_match, bmp_string_to_utf8};

/// Construct UCS-2-BE bytes for an ASCII-only string.
///
/// Each ASCII byte `b` becomes the two-byte sequence `[0x00, b]` since
/// ASCII code points are in the range U+0000..=U+007F and the
/// big-endian UCS-2 encoding of U+00XX is `0x00, 0xXX`.
///
/// Independent oracle: this construction matches the literal
/// description in X.680 Annex B and ITU-T X.690 §8.6 ("BMPString
/// is encoded as a sequence of two-octet code units in big-endian
/// order, with each unit representing one Unicode code point").
fn ucs2_be_ascii(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() * 2);
    for b in s.bytes() {
        assert!(
            b.is_ascii(),
            "ucs2_be_ascii test helper only supports ASCII input"
        );
        v.push(0x00);
        v.push(b);
    }
    v
}

/// `bmp_string_to_utf8` round-trip for ASCII-only input.
///
/// Independent oracle: ASCII Unicode code points (U+0000..=U+007F)
/// are encoded identically as a single byte in UTF-8 and as a
/// two-byte big-endian unit `[0x00, X]` in UCS-2. Decoding the
/// UCS-2 form must yield bytes byte-equal to the original ASCII.
#[test]
fn ascii_round_trip() {
    let src = "Foo Co";
    let ucs2 = ucs2_be_ascii(src);
    let utf8 = bmp_string_to_utf8(&ucs2).expect("well-formed UCS-2 ASCII");
    assert_eq!(utf8, src.as_bytes());
}

/// `bmp_string_to_utf8` for a non-ASCII BMP code point.
///
/// Independent oracle: the Hiragana letter A (U+3042) has well-known
/// encodings — UTF-8: `0xE3 0x81 0x82`; UCS-2-BE: `0x30 0x42`. Both
/// are tabulated in the Unicode Character Database. Citing the
/// Unicode codepoint is the external oracle here, not our own code.
#[test]
fn non_ascii_bmp_code_point() {
    // U+3042 (HIRAGANA LETTER A): UCS-2-BE bytes 0x30 0x42.
    let ucs2 = vec![0x30, 0x42];
    let utf8 = bmp_string_to_utf8(&ucs2).expect("U+3042 is a valid BMP code point");
    // U+3042 in UTF-8 is the well-known three-byte sequence E3 81 82.
    assert_eq!(utf8, vec![0xE3, 0x81, 0x82]);
}

/// Odd-length UCS-2 input is malformed (each unit must be 2 bytes).
/// Fail-closed: return None.
#[test]
fn odd_length_returns_none() {
    let malformed = vec![0x00, 0x46, 0x00]; // 3 bytes, not a multiple of 2
    assert_eq!(bmp_string_to_utf8(&malformed), None);
}

/// UTF-16 surrogate values (U+D800..=U+DFFF) are not valid Unicode
/// scalar values and must not appear in `BMPString`. Fail-closed:
/// return None.
///
/// Independent oracle: Unicode Standard §3.8 explicitly defines
/// surrogates as non-scalar; `core::char::from_u32(0xD800..=0xDFFF)`
/// returns None.
#[test]
fn surrogate_returns_none() {
    // U+D800 (high surrogate, first surrogate code point).
    assert_eq!(bmp_string_to_utf8(&[0xD8, 0x00]), None);
    // U+DC00 (low surrogate).
    assert_eq!(bmp_string_to_utf8(&[0xDC, 0x00]), None);
    // U+DFFF (last surrogate code point).
    assert_eq!(bmp_string_to_utf8(&[0xDF, 0xFF]), None);
}

/// Empty BMPString content is well-formed (zero code points) and
/// transcodes to an empty UTF-8 byte vector.
#[test]
fn empty_input_round_trip() {
    let utf8 = bmp_string_to_utf8(&[]).expect("empty UCS-2 is well-formed (zero units)");
    assert!(utf8.is_empty());
}

/// `ava_values_match`: a BMPString-encoded "Foo Co" must compare
/// equal to a UTF8String-encoded "Foo Co".
///
/// This is the core PKIX-l63j.1 invariant: same Unicode code points
/// in different DER string types compare equal under DN matching.
#[test]
fn bmp_matches_utf8_same_text() {
    let bmp = Any::new(Tag::BmpString, ucs2_be_ascii("Foo Co")).unwrap();
    let utf8 = Any::new(Tag::Utf8String, b"Foo Co".to_vec()).unwrap();
    assert!(ava_values_match(&bmp, &utf8));
    // Symmetry: order of arguments must not matter.
    assert!(ava_values_match(&utf8, &bmp));
}

/// `ava_values_match`: BMPString and PrintableString comparisons
/// must succeed for ASCII content with the same Unicode code points.
#[test]
fn bmp_matches_printable_same_text() {
    let bmp = Any::new(Tag::BmpString, ucs2_be_ascii("Acme CA")).unwrap();
    let printable = Any::new(Tag::PrintableString, b"Acme CA".to_vec()).unwrap();
    assert!(ava_values_match(&bmp, &printable));
    assert!(ava_values_match(&printable, &bmp));
}

/// `ava_values_match`: ASCII case-folding still applies to
/// BMPString-derived UTF-8 bytes after transcoding (since BMP code
/// points U+0041..=U+005A round-trip to ASCII bytes 0x41..=0x5A).
#[test]
fn bmp_matches_utf8_case_insensitive_ascii() {
    let bmp_upper = Any::new(Tag::BmpString, ucs2_be_ascii("FOO CO")).unwrap();
    let utf8_lower = Any::new(Tag::Utf8String, b"foo co".to_vec()).unwrap();
    assert!(ava_values_match(&bmp_upper, &utf8_lower));
}

/// `ava_values_match`: whitespace collapsing still applies to
/// BMPString-derived UTF-8 bytes (BMP space U+0020 → byte 0x20).
#[test]
fn bmp_matches_utf8_whitespace_collapsed() {
    let bmp = Any::new(Tag::BmpString, ucs2_be_ascii("  Foo   Co  ")).unwrap();
    let utf8 = Any::new(Tag::Utf8String, b"foo co".to_vec()).unwrap();
    assert!(ava_values_match(&bmp, &utf8));
}

/// `ava_values_match`: different Unicode content must NOT match
/// across encodings.
#[test]
fn bmp_does_not_match_utf8_different_text() {
    let bmp = Any::new(Tag::BmpString, ucs2_be_ascii("Foo Co")).unwrap();
    let utf8 = Any::new(Tag::Utf8String, b"Bar Co".to_vec()).unwrap();
    assert!(!ava_values_match(&bmp, &utf8));
}

/// `ava_values_match`: malformed BMPString (odd length) is treated
/// as a non-string type by the dispatcher (`any_to_str_bytes` returns
/// None). It must therefore NOT match a well-formed UTF8String of
/// any content. Fail-closed.
#[test]
fn malformed_bmp_does_not_match_well_formed_utf8() {
    // 3-byte content: malformed UCS-2.
    let malformed_bmp = Any::new(Tag::BmpString, vec![0x00, 0x46, 0x00]).unwrap();
    let utf8 = Any::new(Tag::Utf8String, b"F".to_vec()).unwrap();
    assert!(!ava_values_match(&malformed_bmp, &utf8));
    assert!(!ava_values_match(&utf8, &malformed_bmp));
}

/// `ava_values_match`: non-ASCII Unicode code points compare equal
/// when both AVAs encode the same code points (BMPString vs
/// UTF8String).
///
/// Independent oracle: the UCS-2-BE bytes for U+3042 are 0x30 0x42
/// and the UTF-8 bytes are 0xE3 0x81 0x82, both per the Unicode
/// Standard.
#[test]
fn bmp_matches_utf8_non_ascii() {
    // U+3042 HIRAGANA LETTER A.
    let bmp = Any::new(Tag::BmpString, vec![0x30, 0x42]).unwrap();
    let utf8 = Any::new(Tag::Utf8String, vec![0xE3, 0x81, 0x82]).unwrap();
    assert!(ava_values_match(&bmp, &utf8));
}

/// Sanity: two BMPString-encoded values with the same Unicode code
/// points compare equal (this exercises the BMPString-on-both-sides
/// path through `any_to_str_bytes`, where both sides allocate).
#[test]
fn bmp_matches_bmp_same_text() {
    let a = Any::new(Tag::BmpString, ucs2_be_ascii("Acme")).unwrap();
    let b = Any::new(Tag::BmpString, ucs2_be_ascii("acme")).unwrap();
    assert!(ava_values_match(&a, &b));
}
