// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helper functions for serde-serializing DER-encodable types in a
//! format-adaptive wire form.
//!
//! # Wire form
//!
//! Human-readable serializers (JSON, TOML, YAML) receive a **base64**
//! string of the type's DER encoding. Binary serializers (postcard,
//! bincode, MessagePack, CBOR-without-readability-hint) receive the **raw
//! DER bytes**. The selection is driven by
//! [`serde::Serializer::is_human_readable`] /
//! [`serde::Deserializer::is_human_readable`].
//!
//! # Round-trip
//!
//! DER is a canonical encoding for the types this helper is used on
//! (`x509_cert::name::Name`, `x509_cert::serial_number::SerialNumber`,
//! `spki::SubjectPublicKeyInfoOwned`,
//! `x509_cert::ext::pkix::certpolicy::PolicyQualifierInfo`,
//! `der::asn1::ObjectIdentifier`, …). Serialize-then-deserialize produces
//! a value whose DER re-encoding is byte-identical to the original.
//!
//! # Usage
//!
//! ```ignore
//! #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
//! struct Anchor {
//!     #[cfg_attr(
//!         feature = "serde",
//!         serde(with = "crate::serde_der")
//!     )]
//!     subject: x509_cert::name::Name,
//! }
//! ```
//!
//! For `Option<T>` fields, use [`crate::serde_der::option`]. For
//! `Vec<T>` fields, use [`crate::serde_der::vec`].
//!
//! # Why not `serde_with`?
//!
//! [`serde_with`] would express these helpers more concisely via
//! `#[serde_as(as = "DerBase64")]`. We avoid the dependency because the
//! workspace MSRV is 1.73 and the latest `serde_with` 3.x releases
//! (3.18+) require rustc 1.88. Pinning to 3.12 was a viable alternative,
//! but a small hand-written helper module is simpler and keeps the
//! dependency footprint smaller.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use base64ct::{Base64, Encoding};
use der::{Decode, Encode};
use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeSeq};

/// Streams a slice of DER-encodable values as a serde sequence (one element at a time).
struct DerEncodableSeq<'a, T: ?Sized>(&'a [T]);

impl<'a, T> Serialize for DerEncodableSeq<'a, T>
where
    T: Encode,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let is_hr = serializer.is_human_readable();
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for v in self.0 {
            let der_bytes = v.to_der().map_err(serde::ser::Error::custom)?;
            if is_hr {
                let encoded = Base64::encode_string(&der_bytes);
                seq.serialize_element(&encoded)?;
            } else {
                seq.serialize_element(der_bytes.as_slice())?;
            }
        }
        seq.end()
    }
}

/// Serialize a single DER-encodable value.
///
/// Emits a base64 string in human-readable serializers and raw bytes in
/// binary serializers.
///
/// # Errors
///
/// Returns the serializer's error if the underlying serializer fails or
/// if DER encoding of `value` fails.
pub fn serialize<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Encode,
{
    let der_bytes = value.to_der().map_err(serde::ser::Error::custom)?;
    if serializer.is_human_readable() {
        let encoded = Base64::encode_string(&der_bytes);
        serializer.serialize_str(&encoded)
    } else {
        serializer.serialize_bytes(&der_bytes)
    }
}

/// Deserialize a single DER-encodable value.
///
/// Accepts a base64 string from human-readable deserializers and raw
/// bytes from binary deserializers.
///
/// # Errors
///
/// Returns the deserializer's error if the underlying deserializer
/// fails, if base64 decoding fails (human-readable mode), or if DER
/// decoding of the recovered bytes fails.
pub fn deserialize<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: for<'a> Decode<'a>,
{
    let der_bytes = if deserializer.is_human_readable() {
        let s = String::deserialize(deserializer)?;
        Base64::decode_vec(&s).map_err(serde::de::Error::custom)?
    } else {
        Vec::<u8>::deserialize(deserializer)?
    };
    T::from_der(&der_bytes).map_err(serde::de::Error::custom)
}

/// Serde helper for `Option<T>` fields where `T` is DER-encodable.
///
/// Use as `#[serde(with = "crate::serde_der::option")]` on the field.
pub mod option {
    #[cfg(not(feature = "std"))]
    use alloc::{string::String, vec::Vec};

    use super::{Base64, Decode, Deserialize, Deserializer, Encode, Encoding, Serializer};

    /// Serialize an optional DER-encodable value.
    ///
    /// `None` serializes as `null` / unit; `Some(value)` serializes as
    /// `value` per the top-level format rule.
    ///
    /// # Errors
    ///
    /// Returns the serializer's error if the underlying serializer fails
    /// or if DER encoding fails.
    pub fn serialize<S, T>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Encode,
    {
        match value {
            Some(v) => {
                let der_bytes = v.to_der().map_err(serde::ser::Error::custom)?;
                if serializer.is_human_readable() {
                    let encoded = Base64::encode_string(&der_bytes);
                    serializer.serialize_some(&encoded)
                } else {
                    serializer.serialize_some(&der_bytes)
                }
            }
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize an optional DER-encodable value.
    ///
    /// # Errors
    ///
    /// Returns the deserializer's error if the underlying deserializer
    /// fails, if base64 decoding fails, or if DER decoding fails.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: for<'a> Decode<'a>,
    {
        if deserializer.is_human_readable() {
            let opt = Option::<String>::deserialize(deserializer)?;
            match opt {
                Some(s) => {
                    let der_bytes = Base64::decode_vec(&s).map_err(serde::de::Error::custom)?;
                    let v = T::from_der(&der_bytes).map_err(serde::de::Error::custom)?;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        } else {
            let opt = Option::<Vec<u8>>::deserialize(deserializer)?;
            match opt {
                Some(bytes) => {
                    let v = T::from_der(&bytes).map_err(serde::de::Error::custom)?;
                    Ok(Some(v))
                }
                None => Ok(None),
            }
        }
    }
}

/// Serde helper for `Vec<T>` fields where `T` is DER-encodable.
///
/// Use as `#[serde(with = "crate::serde_der::vec")]` on the field. Each
/// element is encoded independently per the top-level format rule.
pub mod vec {
    #[cfg(not(feature = "std"))]
    use alloc::{string::String, vec::Vec};

    use serde::Serialize;

    use super::{
        Base64, Decode, DerEncodableSeq, Deserialize, Deserializer, Encode, Encoding, Serializer,
    };

    /// Serialize a `Vec` of DER-encodable values as a sequence.
    ///
    /// # Errors
    ///
    /// Returns the serializer's error if the underlying serializer fails
    /// or if DER encoding of any element fails.
    pub fn serialize<S, T>(values: &[T], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Encode,
    {
        DerEncodableSeq(values).serialize(serializer)
    }

    /// Deserialize a `Vec` of DER-encodable values from a sequence.
    ///
    /// # Errors
    ///
    /// Returns the deserializer's error if the underlying deserializer
    /// fails, if base64 decoding fails, or if DER decoding of any
    /// element fails.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
    where
        D: Deserializer<'de>,
        T: for<'a> Decode<'a>,
    {
        if deserializer.is_human_readable() {
            let strs = Vec::<String>::deserialize(deserializer)?;
            let mut out = Vec::with_capacity(strs.len());
            for s in strs {
                let der_bytes = Base64::decode_vec(&s).map_err(serde::de::Error::custom)?;
                let v = T::from_der(&der_bytes).map_err(serde::de::Error::custom)?;
                out.push(v);
            }
            Ok(out)
        } else {
            let byte_vecs = Vec::<Vec<u8>>::deserialize(deserializer)?;
            let mut out = Vec::with_capacity(byte_vecs.len());
            for bytes in byte_vecs {
                let v = T::from_der(&bytes).map_err(serde::de::Error::custom)?;
                out.push(v);
            }
            Ok(out)
        }
    }
}

/// Serde helper for `Option<Vec<T>>` fields where `T` is DER-encodable.
///
/// Use as `#[serde(with = "crate::serde_der::option_vec")]` on the
/// field. `None` serializes as `null` / unit; `Some(vec)` serializes as
/// a sequence per [`vec`].
pub mod option_vec {
    #[cfg(not(feature = "std"))]
    use alloc::{string::String, vec::Vec};

    use super::{
        Base64, Decode, DerEncodableSeq, Deserialize, Deserializer, Encode, Encoding, Serializer,
    };

    /// Serialize an optional `Vec` of DER-encodable values.
    ///
    /// # Errors
    ///
    /// Returns the serializer's error if the underlying serializer fails
    /// or if DER encoding fails.
    pub fn serialize<S, T>(value: &Option<Vec<T>>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
        T: Encode,
    {
        match value {
            Some(values) => serializer.serialize_some(&DerEncodableSeq(values)),
            None => serializer.serialize_none(),
        }
    }

    /// Deserialize an optional `Vec` of DER-encodable values.
    ///
    /// # Errors
    ///
    /// Returns the deserializer's error if the underlying deserializer
    /// fails, if base64 decoding fails, or if DER decoding fails.
    pub fn deserialize<'de, D, T>(deserializer: D) -> Result<Option<Vec<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: for<'a> Decode<'a>,
    {
        if deserializer.is_human_readable() {
            let opt = Option::<Vec<String>>::deserialize(deserializer)?;
            match opt {
                Some(strs) => {
                    let mut out = Vec::with_capacity(strs.len());
                    for s in strs {
                        let der_bytes = Base64::decode_vec(&s).map_err(serde::de::Error::custom)?;
                        let v = T::from_der(&der_bytes).map_err(serde::de::Error::custom)?;
                        out.push(v);
                    }
                    Ok(Some(out))
                }
                None => Ok(None),
            }
        } else {
            let opt = Option::<Vec<Vec<u8>>>::deserialize(deserializer)?;
            match opt {
                Some(byte_vecs) => {
                    let mut out = Vec::with_capacity(byte_vecs.len());
                    for bytes in byte_vecs {
                        let v = T::from_der(&bytes).map_err(serde::de::Error::custom)?;
                        out.push(v);
                    }
                    Ok(Some(out))
                }
                None => Ok(None),
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
    const ECDSA_WITH_SHA256_DER: &[u8] =
        &[0x06, 0x08, 0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
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
}
