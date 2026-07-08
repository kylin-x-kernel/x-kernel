// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inter-TA Unix socket wire protocol (`TeeRequest` / `TeeResponse`).
//!
//! **Canonical NOTE/TODO** for the inter-TA wire format and `teec-protocol`
//! migration. Other call sites link here instead of duplicating:
//!
//! - [`tee_ta_manager`](crate::tee::tee_ta_manager)
//! - [`tee_generic`](crate::tee::tee_generic)
//! - `optee_utils/tee_apps/vsock-manager/src/protocol.rs`
//! - `optee_utils/tee_apps/vsock-manager/src/vsock_server.rs`
//!
//! # NOTE: kernel direct path and vsock forward path share this wire format
//!
//! Two entry points reach the same TA listener at `/tmp/{uuid}.sock`:
//!
//! 1. **Kernel direct** — [`tee_ta_manager`](crate::tee::tee_ta_manager) and
//!    [`tee_generic`](crate::tee::tee_generic) serialize messages here and send
//!    them over a kernel Unix socket.
//! 2. **Vsock forward** — `optee_utils/tee_apps/vsock-manager` decodes an
//!    incoming vsock payload with postcard, then forwards the **same postcard
//!    bytes** to `/tmp/{uuid}.sock` without re-encoding.
//!
//! Both paths MUST use identical postcard + serde type definitions and the frame
//! layout below. Keep this module and
//! `optee_utils/tee_apps/vsock-manager/src/protocol.rs` in sync when changing
//! variants or fields.
//!
//! Frame layout: `[4-byte native-endian u32 length][postcard payload]`.
//!
//! # TODO: unify on `teec-protocol`
//!
//! Replace this module with a shared dependency on `teec-protocol` from
//! `xtee-rust-sdk` (`crates/rust-libteec/teec-protocol`) so kernel, vsock-manager,
//! and TA runtime (`xtee-utee`) speak one schema. Until then, the local types here
//! are only kept in sync with `optee_utils/tee_apps/vsock-manager/src/protocol.rs`,
//! **not** with production `TEE_Request` / `TEE_Response`. Known gaps include
//! field names (`raw`/`value` vs `param`/`values`), missing
//! `OpenSession::ca_auth_info`, `ParamType` encoding, socket path
//! (`/tmp/{uuid}.sock` vs `/tmp/{uuid}.{instance_id}.sock`), and the kernel-only
//! [`TeeRequest::Panic`] variant.

use alloc::{string::String, vec::Vec};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::DeserializeOwned};

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub enum TARequest {
    Register { uuid: String },
}

#[derive(Serialize, Deserialize)]
pub enum TeeRequest {
    OpenSession {
        uuid: String,
        connection_method: u32,
        params: Parameters,
    },
    CloseSession {
        session_id: u32,
    },
    InvokeCommand {
        session_id: u32,
        cmd_id: u32,
        params: Parameters,
    },
    RequestCancellation {
        session_id: u32,
    },
    Panic {
        panic_code: u32,
    },
}

#[derive(Serialize, Deserialize)]
pub enum TeeResponse {
    OpenSession { session_id: u32, result: u32 },
    CloseSession { result: u32 },
    InvokeCommand { params: Parameters, result: u32 },
    RequestCancellation { result: u32 },
}

#[derive(Serialize, Deserialize)]
pub struct Parameters(pub Parameter, pub Parameter, pub Parameter, pub Parameter);

impl Parameters {
    pub fn default() -> Self {
        Parameters(
            Parameter::default(),
            Parameter::default(),
            Parameter::default(),
            Parameter::default(),
        )
    }
}

#[derive(Serialize, Deserialize)]
pub struct Parameter {
    pub raw: TEEParam,
    pub param_type: ParamType,
}

impl Parameter {
    pub fn default() -> Self {
        Parameter {
            raw: TEEParam {
                data: Vec::new(),
                value: Value { a: 0, b: 0 },
            },
            param_type: ParamType::None,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct TEEParam {
    pub data: Vec<u8>,
    pub value: Value,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct Value {
    pub a: u32,
    pub b: u32,
}

/// GP TEE parameter type values (serialized as the GP constant, not variant index).
#[derive(Clone, Copy)]
pub enum ParamType {
    None         = 0,
    ValueInput   = 1,
    ValueOutput  = 2,
    ValueInout   = 3,
    MemrefInput  = 5,
    MemrefOutput = 6,
    MemrefInout  = 7,
}

impl Serialize for ParamType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(*self as u32)
    }
}

impl<'de> Deserialize<'de> for ParamType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u32::deserialize(deserializer)? {
            0 => Ok(ParamType::None),
            1 => Ok(ParamType::ValueInput),
            2 => Ok(ParamType::ValueOutput),
            3 => Ok(ParamType::ValueInout),
            5 => Ok(ParamType::MemrefInput),
            6 => Ok(ParamType::MemrefOutput),
            7 => Ok(ParamType::MemrefInout),
            _ => Ok(ParamType::None),
        }
    }
}

impl From<u32> for ParamType {
    fn from(value: u32) -> Self {
        match value {
            0 => ParamType::None,
            1 => ParamType::ValueInput,
            2 => ParamType::ValueOutput,
            3 => ParamType::ValueInout,
            5 => ParamType::MemrefInput,
            6 => ParamType::MemrefOutput,
            7 => ParamType::MemrefInout,
            _ => ParamType::None,
        }
    }
}

/// Serialize a protocol message to postcard bytes.
pub(crate) fn encode_message<T: Serialize>(value: &T) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_allocvec(value)
}

/// Deserialize a protocol message from postcard bytes.
pub(crate) fn decode_message<T: DeserializeOwned>(payload: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(payload)
}

#[cfg(unittest)]
mod tests {
    #[test]
    fn tee_request_open_session_roundtrip() {
        let req = TeeRequest::OpenSession {
            uuid: String::from("936da01f-9abd-4d9d-80c7-02af85c822a8"),
            connection_method: 0,
            params: Parameters::default(),
        };
        let encoded = encode_message(&req).unwrap();
        let decoded: TeeRequest = decode_message(&encoded).unwrap();
        match decoded {
            TeeRequest::OpenSession {
                uuid,
                connection_method,
                ..
            } => {
                assert_eq!(uuid, "936da01f-9abd-4d9d-80c7-02af85c822a8");
                assert_eq!(connection_method, 0);
            }
            _ => panic!("unexpected variant"),
        }
    }

    #[test]
    fn param_type_serializes_gp_values() {
        let param = Parameter {
            raw: TEEParam {
                data: Vec::new(),
                value: Value { a: 0, b: 0 },
            },
            param_type: ParamType::MemrefInput,
        };
        let encoded = encode_message(&param).unwrap();
        let decoded: Parameter = decode_message(&encoded).unwrap();
        assert!(matches!(decoded.param_type, ParamType::MemrefInput));
    }
}
