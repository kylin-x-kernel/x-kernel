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
#[path = "../../tests/host/pkix_serde_der.rs"]
mod tests;
