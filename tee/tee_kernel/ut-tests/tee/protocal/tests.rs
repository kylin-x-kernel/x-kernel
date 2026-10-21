// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[unittest::def_test]
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

#[unittest::def_test]
fn param_type_serializes_gp_values() {
    let param = Parameter {
        raw: TeeParam {
            data: Vec::new(),
            value: Value { a: 0, b: 0 },
        },
        param_type: ParamType::MemrefInput,
    };
    let encoded = encode_message(&param).unwrap();
    let decoded: Parameter = decode_message(&encoded).unwrap();
    assert!(matches!(decoded.param_type, ParamType::MemrefInput));
}
