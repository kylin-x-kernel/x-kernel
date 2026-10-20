// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for the DER serde helpers.
//!
//! These tests use the `der::asn1::ObjectIdentifier` type as the
//! DER-encodable subject because it is small, has a stable encoding,
//! and is independent of the path-validation code under test in the
//! rest of `pkix-path`. The OID `1.2.840.10045.4.3.2`
//! (`ecdsa-with-SHA256`) encodes to the DER bytes
//! `06 08 2A 86 48 CE 3D 04 03 02` per the published ASN.1 encoding
//! rules — this is the *external* oracle: a hand-computed expected
//! encoding, not anything produced by this helper.

use der::asn1::ObjectIdentifier;

use super::*;

const ECDSA_WITH_SHA256_OID: &str = "1.2.840.10045.4.3.2";
// Hand-computed DER encoding of ECDSA_WITH_SHA256_OID. Tag 0x06 (OID),
// length 0x08, then the OID body bytes per X.690 §8.19.
const ECDSA_WITH_SHA256_DER: &[u8] = &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
// Standard base64 (RFC 4648 §4) of the DER bytes, with required
// `=` padding for a 10-byte input. Padding is mandatory in the
// standard alphabet that base64ct's `Base64` type emits and
// requires on decode.
const ECDSA_WITH_SHA256_BASE64: &str = "BggqhkjOPQQDAg==";

/// Serialize a known OID to JSON; assert the emitted JSON matches a
/// hand-computed base64 string. JSON is the human-readable oracle.
#[test]
fn json_serialize_matches_hand_computed_base64() {
    #[derive(Serialize)]
    struct Wrapper {
        #[serde(serialize_with = "super::serialize")]
        oid: ObjectIdentifier,
    }
    let oid: ObjectIdentifier = ECDSA_WITH_SHA256_OID.parse().unwrap();
    let w = Wrapper { oid };
    let json = serde_json::to_string(&w).unwrap();
    let expected = format!(r#"{{"oid":"{}"}}"#, ECDSA_WITH_SHA256_BASE64);
    assert_eq!(json, expected);
}

/// Deserialize a hand-written JSON document; assert the recovered OID
/// equals the known reference value.
#[test]
fn json_deserialize_matches_hand_constructed_value() {
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(deserialize_with = "super::deserialize")]
        oid: ObjectIdentifier,
    }
    let json = format!(r#"{{"oid":"{}"}}"#, ECDSA_WITH_SHA256_BASE64);
    let w: Wrapper = serde_json::from_str(&json).unwrap();
    let expected: ObjectIdentifier = ECDSA_WITH_SHA256_OID.parse().unwrap();
    assert_eq!(w.oid, expected);
}

/// Round-trip OID through JSON; the recovered OID must DER-encode to
/// the same canonical bytes. This is the cache-key requirement from
/// AGENTS.md non-negotiable #6.
#[test]
fn json_round_trip_preserves_der_canonical_form() {
    #[derive(Serialize, Deserialize)]
    struct Wrapper {
        #[serde(with = "super::super::serde_der")]
        oid: ObjectIdentifier,
    }
    let oid: ObjectIdentifier = ECDSA_WITH_SHA256_OID.parse().unwrap();
    let original_der = oid.to_der().unwrap();
    assert_eq!(original_der.as_slice(), ECDSA_WITH_SHA256_DER);

    let w = Wrapper { oid };
    let json = serde_json::to_string(&w).unwrap();
    let back: Wrapper = serde_json::from_str(&json).unwrap();
    let recovered_der = back.oid.to_der().unwrap();
    assert_eq!(recovered_der, original_der);
}

/// Serialize via a binary format (postcard-equivalent custom serializer
/// that reports is_human_readable=false). The helper must emit raw
/// DER bytes, NOT base64. Verified by asserting the serialized
/// `Vec<u8>` payload contains the canonical DER bytes verbatim.
#[test]
fn binary_serializer_emits_raw_der_bytes_not_base64() {
    use serde::ser::Impossible;

    /// Minimal binary serializer that records the bytes written
    /// without any encoding transformation. Reports
    /// `is_human_readable() = false` so the helper takes the binary
    /// branch. Only implements the serializer surface the helper
    /// uses (`serialize_bytes`); everything else panics, which
    /// suffices for this targeted assertion.
    struct BytesCapture {
        captured: Vec<u8>,
    }
    impl Serializer for &mut BytesCapture {
        type Error = serde::de::value::Error;
        type Ok = ();
        type SerializeMap = Impossible<(), Self::Error>;
        type SerializeSeq = Impossible<(), Self::Error>;
        type SerializeStruct = Impossible<(), Self::Error>;
        type SerializeStructVariant = Impossible<(), Self::Error>;
        type SerializeTuple = Impossible<(), Self::Error>;
        type SerializeTupleStruct = Impossible<(), Self::Error>;
        type SerializeTupleVariant = Impossible<(), Self::Error>;

        fn is_human_readable(&self) -> bool {
            false
        }

        fn serialize_bytes(self, v: &[u8]) -> Result<(), Self::Error> {
            self.captured.extend_from_slice(v);
            Ok(())
        }

        fn serialize_bool(self, _: bool) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_i8(self, _: i8) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_i16(self, _: i16) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_i32(self, _: i32) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_i64(self, _: i64) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_u8(self, _: u8) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_u16(self, _: u16) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_u32(self, _: u32) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_u64(self, _: u64) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_f32(self, _: f32) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_f64(self, _: f64) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_char(self, _: char) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_str(self, _: &str) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_none(self) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_unit(self) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_unit_struct(self, _: &'static str) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_unit_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: &T,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<(), Self::Error> {
            unreachable!()
        }

        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
            unreachable!()
        }

        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, Self::Error> {
            unreachable!()
        }

        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, Self::Error> {
            unreachable!()
        }

        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, Self::Error> {
            unreachable!()
        }

        fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
            unreachable!()
        }

        fn serialize_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStruct, Self::Error> {
            unreachable!()
        }

        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, Self::Error> {
            unreachable!()
        }
    }

    let oid: ObjectIdentifier = ECDSA_WITH_SHA256_OID.parse().unwrap();
    let mut cap = BytesCapture {
        captured: Vec::new(),
    };
    super::serialize(&oid, &mut cap).unwrap();
    assert_eq!(cap.captured, ECDSA_WITH_SHA256_DER);
}
