// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, vec, vec::Vec};

    use unittest::def_test;

    use super::{
        ControlFrameError, ControlRequest, ControlRequestFrame, ControlResponse,
        ControlResponseFrame, TEE_ORIGIN_COMMS, TEE_ORIGIN_TEE, TEE_ORIGIN_TRUSTED_APP, TEE_Param,
        TEE_ParamType, TEE_Parameter, TEE_Parameters, TEE_Value,
    };

    /// TEE 结果码（golden fixture 自用；生产值取
    /// `tee_raw_sys::tee_api_defines` 的同名 pub 常量）。
    const TEE_SUCCESS: u32 = 0;
    const TEE_ERROR_COMMUNICATION: u32 = 0xFFFF_000E;
    /// OP-TEE 系代码库权威值（GP 规范文本写 0xFFFF3001，以本代码库为准）。
    const TEE_ERROR_SHORT_BUFFER: u32 = 0xFFFF_0010;

    fn value(a: u32, b: u32, t: TEE_ParamType) -> TEE_Parameter {
        TEE_Parameter {
            param: TEE_Param {
                data: Vec::new(),
                values: TEE_Value { a, b },
            },
            param_type: t,
        }
    }

    fn memref(data: Vec<u8>, a: u32, b: u32, t: TEE_ParamType) -> TEE_Parameter {
        TEE_Parameter {
            param: TEE_Param {
                data,
                values: TEE_Value { a, b },
            },
            param_type: t,
        }
    }

    // -----------------------------------------------------------------------
    // F1–F9 golden fixture（plan 004 §4.4；与 manager 端提交同一组字节）
    // -----------------------------------------------------------------------

    fn f1_request() -> ControlRequestFrame {
        ControlRequestFrame {
            version: 1,
            request: ControlRequest::OpenSession {
                uuid: String::from("936da01f-9abd-4d9d-80c7-02af85c822a8"),
                connection_method: 0,
                params: TEE_Parameters(
                    TEE_Parameter::default(),
                    value(0x1122_3344, 0x5566_7788, TEE_ParamType::ValueInput),
                    memref(vec![0xAA, 0xBB, 0xCC], 0, 0, TEE_ParamType::MemrefInput),
                    memref(Vec::new(), 4096, 0, TEE_ParamType::MemrefOutput),
                ),
            },
        }
    }

    fn f2_request() -> ControlRequestFrame {
        ControlRequestFrame {
            version: 1,
            request: ControlRequest::InvokeCommand {
                session_id: 0x0102_0304,
                cmd_id: 7,
                params: TEE_Parameters(
                    value(1, 2, TEE_ParamType::ValueInout),
                    memref(vec![0x00, 0xFF], 0, 0, TEE_ParamType::MemrefInout),
                    TEE_Parameter::default(),
                    memref(Vec::new(), 61440, 0, TEE_ParamType::MemrefOutput),
                ),
            },
        }
    }

    fn f3_request() -> ControlRequestFrame {
        ControlRequestFrame {
            version: 1,
            request: ControlRequest::CloseSession {
                session_id: 0x0102_0304,
            },
        }
    }

    fn f4_request() -> ControlRequestFrame {
        ControlRequestFrame {
            version: 1,
            request: ControlRequest::ReleaseOwner,
        }
    }

    fn f5_response() -> ControlResponseFrame {
        ControlResponseFrame {
            version: 1,
            response: ControlResponse::OpenSession {
                session_id: 0x0A0B_0C0D,
                result: TEE_SUCCESS,
                origin: TEE_ORIGIN_TEE,
            },
        }
    }

    fn f6_response() -> ControlResponseFrame {
        ControlResponseFrame {
            version: 1,
            response: ControlResponse::InvokeCommand {
                params: TEE_Parameters(
                    memref(vec![0x11; 5], 5, 0, TEE_ParamType::MemrefOutput),
                    value(9, 8, TEE_ParamType::ValueOutput),
                    TEE_Parameter::default(),
                    TEE_Parameter::default(),
                ),
                result: TEE_ERROR_SHORT_BUFFER,
                origin: TEE_ORIGIN_TRUSTED_APP,
            },
        }
    }

    fn f7_response() -> ControlResponseFrame {
        ControlResponseFrame {
            version: 1,
            response: ControlResponse::Error {
                result: TEE_ERROR_COMMUNICATION,
                origin: TEE_ORIGIN_COMMS,
            },
        }
    }

    fn f8_response() -> ControlResponseFrame {
        ControlResponseFrame {
            version: 1,
            response: ControlResponse::CloseSession {
                result: TEE_SUCCESS,
                origin: TEE_ORIGIN_TEE,
            },
        }
    }

    fn f9_response() -> ControlResponseFrame {
        ControlResponseFrame {
            version: 1,
            response: ControlResponse::ReleaseOwner {
                result: TEE_SUCCESS,
                origin: TEE_ORIGIN_TEE,
            },
        }
    }

    // ---- 设计冻结的字节级断言（§4.4「两端都必须附加的显式字节断言」）----

    /// Phase-one extension: fixed bytes shared with the manager mirror.
    #[def_test]
    fn timed_invoke_golden_frame() {
        let request = ControlRequestFrame {
            version: 1,
            request: ControlRequest::InvokeCommandWithTimeout {
                session_id: 7,
                cmd_id: 2,
                params: TEE_Parameters::default(),
                deadline_ns: 1234,
            },
        };
        const GOLDEN: &[u8] = &[
            22, 0, 0, 0, 1, 4, 7, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xd2, 9,
        ];
        assert_eq!(super::encode_control_frame(&request).unwrap(), GOLDEN);
        assert_eq!(
            super::decode_control_frame::<ControlRequestFrame>(GOLDEN).unwrap(),
            request
        );
    }

    /// F9：payload 恰为 `[0x01, 0x04, 0x00, 0x03]`，完整帧为 LE 前缀 + payload，
    /// 解码回 `ReleaseOwner { TEE_SUCCESS, TEE_ORIGIN_TEE }`。
    #[def_test]
    fn test_f9_release_owner_golden_payload_and_frame() {
        let bytes = super::encode_control_frame(&f9_response()).expect("F9 encode");
        assert_eq!(&bytes[4..], &[0x01, 0x04, 0x00, 0x03]);
        assert_eq!(bytes, vec![0x04, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x03]);
        let decoded: ControlResponseFrame = super::decode_control_frame(&bytes).expect("F9 decode");
        assert_eq!(decoded, f9_response());
    }

    /// F1：`version = 1` 编码为单字节 varint `0x01`；第 3 个参数的 `param_type`
    /// 编码字节为 **4**（MemrefInput 的 wire variant index，不是 GP 值 5）。
    /// F1 完整帧（LE u32 长度前缀 + payload）整体作为 golden 常量断言。
    #[def_test]
    fn test_f1_golden_frame_and_memref_wire_index() {
        let bytes = super::encode_control_frame(&f1_request()).expect("F1 encode");
        assert_eq!(bytes, F1_GOLDEN_FRAME);
        let payload = &bytes[4..];
        assert_eq!(payload[0], 0x01, "version=1 必须编码为单字节 varint 0x01");
        let idx = locate_f1_param_type_byte(payload, 2);
        assert_eq!(
            payload[idx], 4,
            "MemrefInput 的 wire param_type 必须是 variant index 4"
        );
        let decoded: ControlRequestFrame = super::decode_control_frame(&bytes).expect("F1 decode");
        assert_eq!(decoded, f1_request());
    }

    /// F1–F4 请求帧 golden：encode(fixture) 必须等于常量，decode(常量) 必须回到
    /// fixture。常量与 manager 端 `control_protocol` 测试模块提交的同一组字节
    /// 逐字节一致（设计 §4.4「两端各自提交固定 golden 常量」）。
    #[def_test]
    fn test_golden_request_frames_f1_to_f4() {
        for (golden, fixture) in [
            (F1_GOLDEN_FRAME, f1_request()),
            (F2_GOLDEN_FRAME, f2_request()),
            (F3_GOLDEN_FRAME, f3_request()),
            (F4_GOLDEN_FRAME, f4_request()),
        ] {
            assert_eq!(
                super::encode_control_frame(&fixture).expect("encode"),
                golden
            );
            let decoded: ControlRequestFrame = super::decode_control_frame(golden).expect("decode");
            assert_eq!(decoded, fixture);
        }
    }

    /// F5–F9 响应帧 golden（同上，响应方向）。
    #[def_test]
    fn test_golden_response_frames_f5_to_f9() {
        for (golden, fixture) in [
            (F5_GOLDEN_FRAME, f5_response()),
            (F6_GOLDEN_FRAME, f6_response()),
            (F7_GOLDEN_FRAME, f7_response()),
            (F8_GOLDEN_FRAME, f8_response()),
            (F9_GOLDEN_FRAME, f9_response()),
        ] {
            assert_eq!(
                super::encode_control_frame(&fixture).expect("encode"),
                golden
            );
            let decoded: ControlResponseFrame =
                super::decode_control_frame(golden).expect("decode");
            assert_eq!(decoded, fixture);
        }
    }

    /// 沿 postcard 编码顺序推进，返回 F1 payload 里第 `param_idx` 个参数的
    /// `param_type` 字节位置（0-based 参数下标）。
    fn locate_f1_param_type_byte(payload: &[u8], want_param: usize) -> usize {
        let uuid = "936da01f-9abd-4d9d-80c7-02af85c822a8";
        let (mut i, _) = varint_u32(payload); // version
        let (uuid_len, _) = varint_u32(&payload[i..]);
        i += uuid_len + uuid.len();
        let (cm_len, _) = varint_u32(&payload[i..]); // connection_method
        i += cm_len;
        for param_idx in 0..4 {
            let (data_len, _) = varint_u32(&payload[i..]);
            i += data_len + data_len;
            let (a_len, _) = varint_u32(&payload[i..]);
            i += a_len;
            let (b_len, _) = varint_u32(&payload[i..]);
            i += b_len;
            if param_idx == want_param {
                return i;
            }
            i += 1; // param_type 单字节
        }
        panic!("param_type byte not found");
    }

    /// 读一个 postcard varint（LE base-128），返回（字节数, 值）。
    fn varint_u32(bytes: &[u8]) -> (usize, u32) {
        let mut value: u32 = 0;
        let mut shift = 0;
        for (n, b) in bytes.iter().enumerate() {
            value |= ((b & 0x7F) as u32) << shift;
            if b & 0x80 == 0 {
                return (n + 1, value);
            }
            shift += 7;
        }
        panic!("varint not terminated");
    }

    // -----------------------------------------------------------------------
    // framing 行为（§4.2）
    // -----------------------------------------------------------------------

    /// 长度前缀 65,537 拒绝、65,536 通过：读侧在分配前即可完成容量判断。
    #[def_test]
    fn test_payload_len_cap_boundary() {
        let oversize: [u8; 4] = 65_537u32.to_le_bytes();
        assert_eq!(
            super::control_frame_payload_len(oversize),
            Err(ControlFrameError::Oversize(65_537))
        );
        let edge: [u8; 4] = 65_536u32.to_le_bytes();
        assert_eq!(super::control_frame_payload_len(edge), Ok(65_536));
    }

    /// 序列化超限帧直接失败（发送侧防线）。实际序列化长度随 envelope 变化，
    /// 断言语义（超上限被拒）而不是精确字节数。
    #[def_test]
    fn test_encode_rejects_oversize_payload() {
        let frame = ControlResponseFrame {
            version: 1,
            response: ControlResponse::InvokeCommand {
                params: TEE_Parameters(
                    memref(
                        vec![0u8; super::MAX_CONTROL_PAYLOAD],
                        0,
                        0,
                        TEE_ParamType::MemrefOutput,
                    ),
                    TEE_Parameter::default(),
                    TEE_Parameter::default(),
                    TEE_Parameter::default(),
                ),
                result: 0,
                origin: 0,
            },
        };
        match super::encode_control_frame(&frame) {
            Err(ControlFrameError::Oversize(len)) => {
                assert!(len > super::MAX_CONTROL_PAYLOAD, "拒绝的长度必须超上限");
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    /// decode 输入不是完整帧或 postcard 解码失败时返回错误（客户端统一映射
    /// COMMUNICATION）。
    #[def_test]
    fn test_decode_rejects_truncated_and_garbage() {
        assert_eq!(
            super::decode_control_frame::<ControlRequestFrame>(&[0x01, 0x02]),
            Err(ControlFrameError::Truncated)
        );
        // 前缀声明 2 字节 payload，实际只给 1 字节。
        assert_eq!(
            super::decode_control_frame::<ControlRequestFrame>(&[0x02, 0x00, 0x00, 0x00, 0xFF]),
            Err(ControlFrameError::Truncated)
        );
        // 前缀合法但 payload 非 postcard。
        let garbage: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0xFF, 0xFF];
        assert_eq!(
            super::decode_control_frame::<ControlRequestFrame>(&garbage),
            Err(ControlFrameError::Deserialize)
        );
    }

    // -----------------------------------------------------------------------
    // F1–F9 完整帧 golden 常量（与 manager 端 vsock-manager/src/control_protocol.rs
    // 测试模块中的同名常量逐字节一致）。
    // -----------------------------------------------------------------------

    const F1_GOLDEN_FRAME: &[u8] = &[
        0x44, 0x00, 0x00, 0x00, // LE u32 长度 = 68
        0x01, // version = 1（单字节 varint）
        0x00, // ControlRequest::OpenSession（variant index 0）
        0x24, // uuid 长度 = 36
        // "936da01f-9abd-4d9d-80c7-02af85c822a8"
        0x39, 0x33, 0x36, 0x64, 0x61, 0x30, 0x31, 0x66, 0x2d, 0x39, 0x61, 0x62, 0x64, 0x2d, 0x34,
        0x64, 0x39, 0x64, 0x2d, 0x38, 0x30, 0x63, 0x37, 0x2d, 0x30, 0x32, 0x61, 0x66, 0x38, 0x35,
        0x63, 0x38, 0x32, 0x32, 0x61, 0x38, 0x00, // connection_method = 0
        // 参数 1：None
        0x00, 0x00, 0x00, 0x00, // 参数 2：ValueInput { a: 0x11223344, b: 0x55667788 }
        0x00, 0xc4, 0xe6, 0x88, 0x89, 0x01, 0x88, 0xef, 0x99, 0xab, 0x05, 0x01,
        // 参数 3：MemrefInput { data: [0xAA, 0xBB, 0xCC] }，wire type = 4
        0x03, 0xaa, 0xbb, 0xcc, 0x00, 0x00, 0x04,
        // 参数 4：MemrefOutput { values.a = 4096 }，wire type = 5
        0x00, 0x80, 0x20, 0x00, 0x05,
    ];

    const F2_GOLDEN_FRAME: &[u8] = &[
        0x1b, 0x00, 0x00, 0x00, 0x01, 0x02, 0x84, 0x86, 0x88, 0x08, 0x07, 0x00, 0x01, 0x02, 0x03,
        0x02, 0x00, 0xff, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0xe0, 0x03, 0x00,
        0x05,
    ];

    const F3_GOLDEN_FRAME: &[u8] = &[0x06, 0x00, 0x00, 0x00, 0x01, 0x01, 0x84, 0x86, 0x88, 0x08];

    const F4_GOLDEN_FRAME: &[u8] = &[0x02, 0x00, 0x00, 0x00, 0x01, 0x03];

    const F5_GOLDEN_FRAME: &[u8] = &[
        0x08, 0x00, 0x00, 0x00, 0x01, 0x01, 0x8d, 0x98, 0xac, 0x50, 0x00, 0x03,
    ];

    const F6_GOLDEN_FRAME: &[u8] = &[
        0x1d, 0x00, 0x00, 0x00, 0x01, 0x03, 0x05, 0x11, 0x11, 0x11, 0x11, 0x11, 0x05, 0x00, 0x05,
        0x00, 0x09, 0x08, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x90, 0x80, 0xfc,
        0xff, 0x0f, 0x04,
    ];

    const F7_GOLDEN_FRAME: &[u8] = &[
        0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x8e, 0x80, 0xfc, 0xff, 0x0f, 0x02,
    ];

    const F8_GOLDEN_FRAME: &[u8] = &[0x04, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x03];

    const F9_GOLDEN_FRAME: &[u8] = &[0x04, 0x00, 0x00, 0x00, 0x01, 0x04, 0x00, 0x03];
