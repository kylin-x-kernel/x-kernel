// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec, vec::Vec};
    use core::ffi::{c_uint, c_ulong};

    use knet::{
        AcceptOptions, SocketAddrEx, SocketOps,
        unix::{StreamTransport, UnixAddr, UnixDomainSocket, UnixSocketType},
    };
    use ksync::Mutex;
    use ktask::KtaskRef;
    use posix_types::UserConstPtr;
    use tee_raw_sys::{
        TEE_ERROR_ACCESS_DENIED, TEE_ERROR_BAD_FORMAT, TEE_ERROR_BAD_PARAMETERS,
        TEE_ERROR_COMMUNICATION, TEE_ERROR_ITEM_NOT_FOUND, TEE_ERROR_SHORT_BUFFER, TEE_ORIGIN_API,
        TEE_ORIGIN_COMMS, TEE_ORIGIN_TEE, TEE_ORIGIN_TRUSTED_APP, TEE_SUCCESS, TEE_UUID,
        utee_params,
    };
    use unittest::{assert_eq, def_test};

    use super::{
        INTER_TA_PARAM_BUDGET, ParamWriteback, apply_response_params, param_at,
        sys_tee_scn_close_ta_session, sys_tee_scn_invoke_ta_command, sys_tee_scn_open_ta_session,
        wire_params_from_user,
    };
    use crate::{
        TestUserBuffer,
        tee::{
            control_protocol::{
                CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestFrame, ControlResponse,
                ControlResponseFrame, TEE_Param, TEE_ParamType, TEE_Parameter, TEE_Parameters,
                TEE_Value,
            },
            tee_session::{with_tee_ta_ctx, with_tee_ta_ctx_mut},
            tee_ta_manager::CONTROL_SOCKET_PATH,
        },
    };

    // -----------------------------------------------------------------------
    // 用户侧 utee_params 构造:repr(C) 72 字节(types u64 + 8 个 u64 槽),
    // 按字节布局写入用户缓冲,再由 syscall 侧 read_vm 重建。
    // -----------------------------------------------------------------------

    /// (nibble, a, b) per param index。
    type ParamSpec = [(u64, u64, u64); 4];

    fn build_user_params(specs: ParamSpec) -> (TestUserBuffer, *mut utee_params) {
        let mut raw = [0u8; 72];
        let mut types: u64 = 0;
        for (index, (nibble, a, b)) in specs.iter().enumerate() {
            types |= nibble << (index * 4);
            raw[8 + index * 16..8 + index * 16 + 8].copy_from_slice(&a.to_le_bytes());
            raw[16 + index * 16..16 + index * 16 + 8].copy_from_slice(&b.to_le_bytes());
        }
        raw[0..8].copy_from_slice(&types.to_le_bytes());
        let buf = TestUserBuffer::new(72).expect("alloc user params");
        buf.write_bytes(&raw).expect("write user params");
        let ptr = buf.as_user_ptr::<utee_params>();
        (buf, ptr)
    }

    fn read_back_params(ptr: *mut utee_params) -> utee_params {
        UserConstPtr::<utee_params>::from(ptr.cast_const())
            .read_vm()
            .expect("read back params")
    }

    fn read_params_value(ptr: *mut utee_params, index: usize) -> (u64, u64) {
        read_back_params(ptr).value(index)
    }

    // -----------------------------------------------------------------------
    // §7.2/§7.3 转换测试
    // -----------------------------------------------------------------------

    #[def_test(user)]
    fn test_wire_params_none() {
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let up = read_back_params(ptr);
        let params = wire_params_from_user(&up).expect("none converts");
        for index in 0..4 {
            assert_eq!(param_at(&params, index).param_type, TEE_ParamType::None);
            assert!(param_at(&params, index).param.data.is_empty());
        }
        drop(buf);
    }

    #[def_test(user)]
    fn test_wire_params_value_takes_low32() {
        let (buf, ptr) = build_user_params([
            (1, 0x1_0000_00AB, 0x1_0000_00CD),
            (3, 7, 8),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let params = wire_params_from_user(&read_back_params(ptr)).expect("value converts");
        assert_eq!(param_at(&params, 0).param_type, TEE_ParamType::ValueInput);
        assert_eq!(param_at(&params, 0).param.values.a, 0xAB);
        assert_eq!(param_at(&params, 0).param.values.b, 0xCD);
        assert_eq!(param_at(&params, 1).param_type, TEE_ParamType::ValueInout);
        assert_eq!(param_at(&params, 1).param.values.a, 7);
        assert_eq!(param_at(&params, 1).param.values.b, 8);
        drop(buf);
    }

    #[def_test(user)]
    fn test_wire_params_memref_input_copies_user_data() {
        let data = TestUserBuffer::new(4).expect("alloc data");
        data.write_bytes(b"abcd").expect("write data");
        let (buf, ptr) = build_user_params([
            (5, data.as_user_ptr::<u8>() as u64, 4),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let params = wire_params_from_user(&read_back_params(ptr)).expect("memref converts");
        assert_eq!(param_at(&params, 0).param_type, TEE_ParamType::MemrefInput);
        assert_eq!(param_at(&params, 0).param.data.as_slice(), b"abcd");
        drop(buf);
        drop(data);
    }

    #[def_test(user)]
    fn test_wire_params_memref_null_addr_zero_len_is_valid() {
        let (buf, ptr) = build_user_params([(5, 0, 0), (6, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let params = wire_params_from_user(&read_back_params(ptr)).expect("null+b=0 converts");
        assert!(param_at(&params, 0).param.data.is_empty());
        drop(buf);
    }

    #[def_test(user)]
    fn test_wire_params_memref_null_addr_nonzero_len_rejected() {
        let (buf, ptr) = build_user_params([(5, 0, 4), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        assert_eq!(
            wire_params_from_user(&read_back_params(ptr))
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_PARAMETERS
        );
        drop(buf);
    }

    #[def_test(user)]
    fn test_wire_params_illegal_nibbles_rejected() {
        // 4 与 8 都不在 GP 类型集合中。
        for nibble in [4u64, 8, 9, 15] {
            let (buf, ptr) = build_user_params([(nibble, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
            assert_eq!(
                wire_params_from_user(&read_back_params(ptr))
                    .unwrap_err()
                    .result,
                TEE_ERROR_BAD_PARAMETERS,
                "nibble {nibble} must be rejected"
            );
            drop(buf);
        }
    }

    #[def_test(user)]
    fn test_wire_params_input_budget_boundary() {
        let data = TestUserBuffer::new(INTER_TA_PARAM_BUDGET).expect("alloc data");
        // 单参数 60 KiB 恰好达标;两个 60 KiB 超预算。
        let (one, ptr_one) = build_user_params([
            (
                5,
                data.as_user_ptr::<u8>() as u64,
                INTER_TA_PARAM_BUDGET as u64,
            ),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        assert!(wire_params_from_user(&read_back_params(ptr_one)).is_ok());
        drop(one);

        let (two, ptr_two) = build_user_params([
            (
                5,
                data.as_user_ptr::<u8>() as u64,
                INTER_TA_PARAM_BUDGET as u64,
            ),
            (5, data.as_user_ptr::<u8>() as u64, 1),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        assert_eq!(
            wire_params_from_user(&read_back_params(ptr_two))
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_PARAMETERS
        );
        drop(two);
        drop(data);
    }

    #[def_test(user)]
    fn test_wire_params_output_capacity_budget() {
        // 两个 MEMREF_OUTPUT 容量合计 60 KiB + 1 → 拒绝;均无需用户缓冲。
        let (over, ptr_over) = build_user_params([
            (6, 0x1000, 30 * 1024u64 + 1),
            (6, 0x2000, 30 * 1024u64),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        assert_eq!(
            wire_params_from_user(&read_back_params(ptr_over))
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_PARAMETERS
        );
        drop(over);

        let (within, ptr_within) = build_user_params([
            (6, 0x1000, 30 * 1024u64),
            (6, 0x2000, 30 * 1024u64),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let params = wire_params_from_user(&read_back_params(ptr_within)).expect("within budget");
        // §7.2:容量经 values.a 上 wire。
        assert_eq!(param_at(&params, 0).param.values.a, 30 * 1024u32);
        drop(within);
    }

    #[def_test(user)]
    fn test_wire_params_len_overflow_rejected() {
        let (buf, ptr) =
            build_user_params([(5, 0x1000, u64::MAX), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        assert_eq!(
            wire_params_from_user(&read_back_params(ptr))
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_PARAMETERS
        );
        drop(buf);
    }

    // -----------------------------------------------------------------------
    // §7.4 响应应用测试
    // -----------------------------------------------------------------------

    fn wire_value(param_type: TEE_ParamType, a: u32, b: u32) -> TEE_Parameter {
        TEE_Parameter {
            param: TEE_Param {
                data: Vec::new(),
                values: TEE_Value { a, b },
            },
            param_type,
        }
    }

    fn wire_memref(param_type: TEE_ParamType, data: Vec<u8>, a: u32) -> TEE_Parameter {
        TEE_Parameter {
            param: TEE_Param {
                data,
                values: TEE_Value { a, b: 0 },
            },
            param_type,
        }
    }

    fn wire_parameters(params: [TEE_Parameter; 4]) -> TEE_Parameters {
        TEE_Parameters(
            params[0].clone(),
            params[1].clone(),
            params[2].clone(),
            params[3].clone(),
        )
    }

    #[def_test(user)]
    fn test_apply_value_output_on_success() {
        let (buf, ptr) = build_user_params([(2, 0, 0), (3, 1, 2), (0, 0, 0), (0, 0, 0)]);
        let mut up = read_back_params(ptr);
        let response = wire_parameters([
            wire_value(TEE_ParamType::ValueOutput, 7, 8),
            wire_value(TEE_ParamType::ValueInout, 9, 10),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let outcome = apply_response_params(&mut up, &response, TEE_SUCCESS).expect("apply ok");
        assert_eq!(outcome, ParamWriteback::OutputsApplied);
        assert_eq!(up.value(0), (7, 8));
        assert_eq!(up.value(1), (9, 10));
        drop(buf);
    }

    #[def_test(user)]
    fn test_apply_value_output_not_written_on_error() {
        let (buf, ptr) = build_user_params([(2, 3, 4), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let mut up = read_back_params(ptr);
        let response = wire_parameters([
            wire_value(TEE_ParamType::ValueOutput, 7, 8),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let outcome =
            apply_response_params(&mut up, &response, TEE_ERROR_ACCESS_DENIED).expect("apply ok");
        assert_eq!(outcome, ParamWriteback::NoOutputs);
        // 值对保持调用方原值(§7.4:VALUE 仅成功时写回)。
        assert_eq!(up.value(0), (3, 4));
        drop(buf);
    }

    #[def_test(user)]
    fn test_apply_memref_output_on_success() {
        let out = TestUserBuffer::new(8).expect("alloc out");
        out.write_bytes(&[0xAA; 8]).expect("fill out");
        let (buf, ptr) = build_user_params([
            (6, out.as_user_ptr::<u8>() as u64, 8),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let mut up = read_back_params(ptr);
        let response = wire_parameters([
            wire_memref(TEE_ParamType::MemrefOutput, b"resp".to_vec(), 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let outcome = apply_response_params(&mut up, &response, TEE_SUCCESS).expect("apply ok");
        assert_eq!(outcome, ParamWriteback::OutputsApplied);
        assert_eq!(out.read_bytes(4).unwrap(), b"resp".to_vec());
        // 长度写为 data 实际长度。
        assert_eq!(up.value(0), (out.as_user_ptr::<u8>() as u64, 4));
        drop(buf);
        drop(out);
    }

    #[def_test(user)]
    fn test_apply_kernel_short_buffer_when_data_exceeds_capacity() {
        let out = TestUserBuffer::new(4).expect("alloc out");
        out.write_bytes(&[0x11; 4]).expect("fill out");
        let (buf, ptr) = build_user_params([
            (6, out.as_user_ptr::<u8>() as u64, 4),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let mut up = read_back_params(ptr);
        let response = wire_parameters([
            // 13 字节 > 容量 4:内核检测的 SHORT_BUFFER 路径。
            wire_memref(TEE_ParamType::MemrefOutput, b"over-capacity".to_vec(), 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let outcome = apply_response_params(&mut up, &response, TEE_SUCCESS).expect("apply ok");
        assert_eq!(outcome, ParamWriteback::ShortBuffer);
        // 不复制任何 data;长度改为该参数 response data 的实际长度。
        assert_eq!(out.read_bytes(4).unwrap(), vec![0x11u8; 4]);
        assert_eq!(up.value(0), (out.as_user_ptr::<u8>() as u64, 13));
        drop(buf);
        drop(out);
    }

    #[def_test(user)]
    fn test_apply_ta_short_buffer_reports_required_len() {
        let out = TestUserBuffer::new(8).expect("alloc out");
        let (buf, ptr) = build_user_params([
            (6, out.as_user_ptr::<u8>() as u64, 8),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let mut up = read_back_params(ptr);
        // TA 主动 SHORT_BUFFER:required len 放在 values.a。
        let response = wire_parameters([
            wire_memref(TEE_ParamType::MemrefOutput, b"resp".to_vec(), 100),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let outcome =
            apply_response_params(&mut up, &response, TEE_ERROR_SHORT_BUFFER).expect("apply ok");
        assert_eq!(outcome, ParamWriteback::ShortBuffer);
        // required length 写回,数据不复制(即使 4 <= 8)。
        assert_eq!(up.value(0), (out.as_user_ptr::<u8>() as u64, 100));
        assert_eq!(out.read_bytes(8).unwrap(), vec![0u8; 8]);
        drop(buf);
        drop(out);
    }

    #[def_test(user)]
    fn test_apply_ta_short_buffer_malformed_required_len() {
        let (buf, ptr) = build_user_params([(6, 0x1000, 8), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let mut up = read_back_params(ptr);
        // required < data.len → malformed response。
        let response = wire_parameters([
            wire_memref(TEE_ParamType::MemrefOutput, b"resp".to_vec(), 2),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        assert_eq!(
            apply_response_params(&mut up, &response, TEE_ERROR_SHORT_BUFFER)
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_FORMAT
        );
        // required > 60 KiB 同样 malformed。
        let response = wire_parameters([
            wire_memref(TEE_ParamType::MemrefOutput, b"r".to_vec(), 61 * 1024u32),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        assert_eq!(
            apply_response_params(&mut up, &response, TEE_ERROR_SHORT_BUFFER)
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_FORMAT
        );
        drop(buf);
    }

    #[def_test(user)]
    fn test_apply_type_mismatch_is_bad_format() {
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let mut up = read_back_params(ptr);
        let response = wire_parameters([
            wire_value(TEE_ParamType::ValueInput, 1, 2),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        let failure = apply_response_params(&mut up, &response, TEE_SUCCESS).unwrap_err();
        assert_eq!(failure.result, TEE_ERROR_BAD_FORMAT);
        // §6.7:malformed response 属传输/协议类,origin 恒 COMMS。
        assert_eq!(failure.origin, TEE_ORIGIN_COMMS);
        drop(buf);
    }

    #[def_test(user)]
    fn test_apply_response_data_budget() {
        let out = TestUserBuffer::new(INTER_TA_PARAM_BUDGET).expect("alloc out");
        let (buf, ptr) = build_user_params([
            (
                6,
                out.as_user_ptr::<u8>() as u64,
                INTER_TA_PARAM_BUDGET as u64,
            ),
            (6, out.as_user_ptr::<u8>() as u64, 1),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let mut up = read_back_params(ptr);
        // 两个参数 data 合计 60 KiB + 1 → malformed response。
        let response = wire_parameters([
            wire_memref(
                TEE_ParamType::MemrefOutput,
                vec![0; INTER_TA_PARAM_BUDGET],
                0,
            ),
            wire_memref(TEE_ParamType::MemrefOutput, b"x".to_vec(), 0),
            wire_value(TEE_ParamType::None, 0, 0),
            wire_value(TEE_ParamType::None, 0, 0),
        ]);
        assert_eq!(
            apply_response_params(&mut up, &response, TEE_SUCCESS)
                .unwrap_err()
                .result,
            TEE_ERROR_BAD_FORMAT
        );
        drop(buf);
        drop(out);
    }

    // -----------------------------------------------------------------------
    // §6.4/§6.5/§6.6 syscall 流程(mock manager,固定控制面路径,串行)
    // -----------------------------------------------------------------------

    /// 脚本化 mock manager:按脚本顺序 accept 连接,记录请求帧并应答。
    enum ScriptedReply {
        Frame(Vec<u8>),
        /// 应答前关闭连接(EOF 路径)。
        Close,
    }

    use crate::tee::tee_ta_manager::control_test_support::{server_recv_frame, server_send_all};

    fn spawn_scripted_control_server(
        path: &str,
        replies: Vec<ScriptedReply>,
        captured: Arc<Mutex<Vec<Vec<u8>>>>,
    ) -> KtaskRef {
        ensure_socket_node_absent(path);
        let listener = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
        listener
            .bind(SocketAddrEx::Unix(UnixAddr::Path(path.into())))
            .expect("bind scripted server");
        listener
            .listen(replies.len().max(1))
            .expect("listen scripted server");
        ktask::spawn(move || {
            for reply in replies {
                let Ok(accepted) = listener.accept(AcceptOptions::default()) else {
                    return;
                };
                let knet::Socket::Unix(conn) = accepted else {
                    return;
                };
                let frame = server_recv_frame(&conn);
                captured.lock().push(frame);
                if let ScriptedReply::Frame(bytes) = reply {
                    server_send_all(&conn, &bytes);
                }
                // Close:直接进入下一次迭代,conn 析构 → 客户端 EOF。
            }
        })
    }

    fn response_frame(response: ControlResponse) -> Vec<u8> {
        crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
            version: CONTROL_PROTOCOL_VERSION,
            response,
        })
        .expect("encode response")
    }

    fn seed_session(target_uuid: &str, manager_session_id: u32) -> u32 {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(ctx
                .insert_session(String::from(target_uuid), manager_session_id)
                .expect("seed session"))
        })
        .expect("seed session ctx")
    }

    fn session_count() -> usize {
        with_tee_ta_ctx(|ctx| Ok(ctx.open_sessions.len())).expect("count sessions")
    }

    /// 任意内容(非全零)的 UUID 用户缓冲:syscall 侧 read_vm 重建。
    /// 缓冲区由调用方持有,避免返回悬垂指针。
    fn test_uuid() -> (crate::TestUserValue<TEE_UUID>, *const TEE_UUID) {
        let value = TEE_UUID {
            timeLow: 0x936DA01F,
            timeMid: 0x9ABD,
            timeHiAndVersion: 0x4D9D,
            clockSeqAndNode: [0x80, 0xC7, 0x02, 0xAF, 0x85, 0xC8, 0x22, 0xA8],
        };
        let holder = crate::TestUserValue::<TEE_UUID>::from_value(value).expect("alloc uuid");
        let ptr = holder.as_user_ptr();
        (holder, ptr)
    }

    fn decode_request(frame: &[u8]) -> ControlRequestFrame {
        crate::tee::control_protocol::decode_control_frame(frame).expect("decode request")
    }

    #[def_test(user)]
    fn test_open_rejects_null_session_out() {
        // 无 socket:写回地址 NULL 在交换前失败。
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        let (_uuid, uuid_ptr) = test_uuid();
        assert_eq!(
            sys_tee_scn_open_ta_session(
                uuid_ptr,
                0,
                ptr,
                core::ptr::null_mut(),
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_API);
    }

    #[def_test(user)]
    fn test_open_rejects_invalid_uuid_ptr() {
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let session_out = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        assert_eq!(
            sys_tee_scn_open_ta_session(
                core::ptr::null(),
                0,
                ptr,
                session_out.as_user_ptr(),
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_API);
    }

    #[def_test(user, serial)]
    fn test_open_success_returns_local_handle() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::OpenSession {
                    session_id: 11,
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            captured.clone(),
        );
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let session_out = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        let (_uuid, uuid_ptr) = test_uuid();
        assert_eq!(
            sys_tee_scn_open_ta_session(
                uuid_ptr,
                0,
                ptr,
                session_out.as_user_ptr(),
                ret_orig.as_user_ptr(),
            ),
            Ok(())
        );
        // §6.2:handle 从 1 开始,永不 0。
        assert_eq!(session_out.read(), 1);
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TEE);
        server.join();

        let requests = captured.lock().clone();
        assert_eq!(requests.len(), 1);
        let decoded = decode_request(&requests[0]);
        let ControlRequestFrame {
            version,
            request:
                ControlRequest::OpenSession {
                    connection_method, ..
                },
        } = decoded
        else {
            panic!("expected OpenSession request");
        };
        assert_eq!(version, CONTROL_PROTOCOL_VERSION);
        assert_eq!(connection_method, 0);
    }

    #[def_test(user, serial)]
    fn test_open_manager_error_maps_wire_origin() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::OpenSession {
                    session_id: 0,
                    result: TEE_ERROR_ACCESS_DENIED,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                },
            ))],
            captured,
        );
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let session_out = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        let (_uuid, uuid_ptr) = test_uuid();
        assert_eq!(
            sys_tee_scn_open_ta_session(
                uuid_ptr,
                0,
                ptr,
                session_out.as_user_ptr(),
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ACCESS_DENIED)
        );
        // §6.7:origin 取 wire 响应,不猜测。
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TRUSTED_APP);
        // 失败不留本地 handle。
        assert_eq!(session_count(), 0);
        server.join();
    }

    #[def_test(user, serial)]
    fn test_open_rolls_back_when_session_out_write_fails() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        // 第一条:OpenSession 成功;第二条:回滚触发的 CloseSession。
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![
                ScriptedReply::Frame(response_frame(ControlResponse::OpenSession {
                    session_id: 11,
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                })),
                ScriptedReply::Frame(response_frame(ControlResponse::CloseSession {
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                })),
            ],
            captured.clone(),
        );
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        // 非空但未映射的用户地址:check_non_null 通过,write_vm 失败。
        let bad_session_out = 0x1000 as *mut c_uint;
        let (_uuid, uuid_ptr) = test_uuid();
        assert_eq!(
            sys_tee_scn_open_ta_session(uuid_ptr, 0, ptr, bad_session_out, ret_orig.as_user_ptr(),),
            Err(TEE_ERROR_BAD_PARAMETERS)
        );
        server.join();

        // §6.4:不留本地 handle,并向 manager 发送 CloseSession 回滚。
        assert_eq!(session_count(), 0);
        let requests = captured.lock().clone();
        assert_eq!(requests.len(), 2);
        let decoded = decode_request(&requests[1]);
        let ControlRequestFrame {
            request: ControlRequest::CloseSession { session_id },
            ..
        } = decoded
        else {
            panic!("expected rollback CloseSession");
        };
        assert_eq!(session_id, 11);
    }

    #[def_test(user, serial)]
    fn timed_invoke_forwards_absolute_deadline_and_target_cancel() {
        use tee_raw_sys::TEE_ERROR_CANCEL;
        for timeout_ms in [0, 30] {
            let captured = Arc::new(Mutex::new(Vec::new()));
            let server = spawn_scripted_control_server(
                CONTROL_SOCKET_PATH,
                vec![ScriptedReply::Frame(response_frame(
                    ControlResponse::InvokeCommand {
                        params: TEE_Parameters::default(),
                        result: TEE_ERROR_CANCEL,
                        origin: TEE_ORIGIN_TRUSTED_APP,
                    },
                ))],
                captured.clone(),
            );
            let handle = seed_session("timed-invoke", 11);
            let (_buf, ptr) = build_user_params([(0, 0, 0); 4]);
            let origin = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
            let before = ktask::monotonic_time().as_nanos_u64_saturating();
            assert_eq!(
                sys_tee_scn_invoke_ta_command(
                    handle as c_ulong,
                    timeout_ms,
                    1,
                    ptr,
                    origin.as_user_ptr()
                ),
                Err(TEE_ERROR_CANCEL)
            );
            assert_eq!(origin.read(), TEE_ORIGIN_TRUSTED_APP);
            server.join();
            let requests = captured.lock();
            let ControlRequestFrame {
                request: ControlRequest::InvokeCommandWithTimeout { deadline_ns, .. },
                ..
            } = decode_request(&requests[0])
            else {
                panic!("expected timed Invoke")
            };
            let timeout_ns = timeout_ms * 1_000_000;
            assert!(deadline_ns >= before + timeout_ns);
            assert!(deadline_ns <= ktask::monotonic_time().as_nanos_u64_saturating() + timeout_ns);
        }
    }

    #[def_test(user)]
    fn test_invoke_unknown_handle_fails_without_socket() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .unwrap();
        let (_buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        assert_eq!(
            sys_tee_scn_invoke_ta_command(7, 0, 1, ptr, ret_orig.as_user_ptr()),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_API);
    }

    #[def_test(user, serial)]
    fn test_invoke_success_writes_outputs() {
        let handle = seed_session("uuid-invoke", 11);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let mem_out = TestUserBuffer::new(8).unwrap();
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::InvokeCommand {
                    params: wire_parameters([
                        wire_value(TEE_ParamType::ValueInput, 0x12, 0x34),
                        wire_value(TEE_ParamType::ValueOutput, 7, 8),
                        wire_memref(TEE_ParamType::MemrefOutput, b"resp".to_vec(), 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                    ]),
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            captured.clone(),
        );
        // 请求:VALUE_INPUT + VALUE_OUTPUT + MEMREF_OUTPUT。
        let (buf, ptr) = build_user_params([
            (1, 0x12, 0x34),
            (2, 0, 0),
            (6, mem_out.as_user_ptr::<u8>() as u64, 8),
            (0, 0, 0),
        ]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                0x77,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Ok(())
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TEE);
        // 输出值对与 memref data 写回用户内存。
        assert_eq!(read_params_value(ptr, 1), (7, 8));
        assert_eq!(
            read_params_value(ptr, 2),
            (mem_out.as_user_ptr::<u8>() as u64, 4)
        );
        assert_eq!(mem_out.read_bytes(4).unwrap(), b"resp".to_vec());
        server.join();

        // 请求帧:global session id + cmd id + 转换后的参数。
        let requests = captured.lock().clone();
        let decoded = decode_request(&requests[0]);
        let ControlRequestFrame {
            request:
                ControlRequest::InvokeCommand {
                    session_id,
                    cmd_id,
                    params,
                },
            ..
        } = decoded
        else {
            panic!("expected InvokeCommand request");
        };
        assert_eq!(session_id, 11);
        assert_eq!(cmd_id, 0x77);
        assert_eq!(param_at(&params, 0).param.values.a, 0x12);
        drop(buf);
        drop(mem_out);
    }

    #[def_test(user, serial)]
    fn test_invoke_item_not_found_deletes_entry() {
        let handle = seed_session("uuid-invoke-2", 12);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::InvokeCommand {
                    params: wire_parameters([
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                    ]),
                    result: TEE_ERROR_ITEM_NOT_FOUND,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            captured,
        );
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TEE);
        // §6.5:失效 handle 从本地表删除。
        assert_eq!(session_count(), 0);
        server.join();
        drop(buf);
    }

    #[def_test(user, serial)]
    fn test_invoke_ta_item_not_found_keeps_session_for_invoke_and_close() {
        let handle = seed_session("uuid-ta-item-not-found", 21);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let params = wire_parameters(core::array::from_fn(|_| {
            wire_value(TEE_ParamType::None, 0, 0)
        }));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![
                ScriptedReply::Frame(response_frame(ControlResponse::InvokeCommand {
                    params: params.clone(),
                    result: TEE_ERROR_ITEM_NOT_FOUND,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                })),
                ScriptedReply::Frame(response_frame(ControlResponse::InvokeCommand {
                    params,
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                })),
                ScriptedReply::Frame(response_frame(ControlResponse::CloseSession {
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                })),
            ],
            Arc::clone(&captured),
        );
        let (_buf, ptr) = build_user_params([(0, 0, 0); 4]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TRUSTED_APP);
        assert_eq!(
            session_count(),
            1,
            "a missing TA object does not close its session"
        );
        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                2,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Ok(())
        );
        assert_eq!(sys_tee_scn_close_ta_session(handle as c_ulong), Ok(()));
        assert_eq!(session_count(), 0);
        server.join();
        assert_eq!(captured.lock().len(), 3);
    }

    #[def_test(user, serial)]
    fn test_invoke_access_denied_ta_refusal_keeps_entry() {
        let handle = seed_session("uuid-invoke-denied", 19);
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::InvokeCommand {
                    params: wire_parameters([
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                    ]),
                    // §6.7 行 4:目标 TA 的命令级拒绝是 ACCESS_DENIED +
                    // TRUSTED_APP;session 仍存活,本地表项必须保留。
                    result: TEE_ERROR_ACCESS_DENIED,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                },
            ))],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ACCESS_DENIED)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TRUSTED_APP);
        assert_eq!(session_count(), 1, "TA refusal must keep the local entry");
        server.join();
        drop(buf);
    }

    #[def_test(user, serial)]
    fn test_invoke_owner_mismatch_deletes_entry() {
        let handle = seed_session("uuid-invoke-owner", 20);
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::InvokeCommand {
                    params: wire_parameters([
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                    ]),
                    // §5.5 步骤 3:owner 失配 = ACCESS_DENIED + ORIGIN_API,
                    // 失效 handle 必须删除。
                    result: TEE_ERROR_ACCESS_DENIED,
                    origin: TEE_ORIGIN_API,
                },
            ))],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ACCESS_DENIED)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_API);
        assert_eq!(
            session_count(),
            0,
            "owner mismatch must delete the stale local entry"
        );
        server.join();
        drop(buf);
    }

    #[def_test(user, serial)]
    fn test_invoke_ta_short_buffer_passes_origin() {
        let handle = seed_session("uuid-invoke-3", 13);
        let mem_out = TestUserBuffer::new(8).unwrap();
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::InvokeCommand {
                    params: wire_parameters([
                        wire_memref(TEE_ParamType::MemrefOutput, b"resp".to_vec(), 100),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                        wire_value(TEE_ParamType::None, 0, 0),
                    ]),
                    result: TEE_ERROR_SHORT_BUFFER,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                },
            ))],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (buf, ptr) = build_user_params([
            (6, mem_out.as_user_ptr::<u8>() as u64, 8),
            (0, 0, 0),
            (0, 0, 0),
            (0, 0, 0),
        ]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_SHORT_BUFFER)
        );
        // TA 主动 SHORT_BUFFER:透传 TA origin;required len 写回 b 槽。
        assert_eq!(ret_orig.read(), TEE_ORIGIN_TRUSTED_APP);
        assert_eq!(
            read_params_value(ptr, 0),
            (mem_out.as_user_ptr::<u8>() as u64, 100)
        );
        // 数据不复制。
        assert_eq!(mem_out.read_bytes(8).unwrap(), vec![0u8; 8]);
        server.join();
        drop(buf);
        drop(mem_out);
    }

    #[def_test(user, serial)]
    fn test_invoke_manager_eof_maps_communication() {
        let handle = seed_session("uuid-invoke-eof", 18);
        // 应答前关闭连接:客户端 Eof → COMMUNICATION,不自动重试。
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Close],
            Arc::new(Mutex::new(Vec::new())),
        );
        let (buf, ptr) = build_user_params([(0, 0, 0), (0, 0, 0), (0, 0, 0), (0, 0, 0)]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();

        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_COMMUNICATION)
        );
        // §6.7:传输失败 origin 恒 COMMS;表项保留(不在删除清单)。
        assert_eq!(ret_orig.read(), TEE_ORIGIN_COMMS);
        assert_eq!(session_count(), 1);
        server.join();
        drop(buf);
    }

    use crate::tee::tee_ta_manager::control_test_support::ensure_socket_node_absent;

    #[def_test(user)]
    fn test_close_unknown_handle_fails_without_socket() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .unwrap();
        assert_eq!(
            sys_tee_scn_close_ta_session(9),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
    }

    #[def_test(user, serial)]
    fn test_close_success_removes_entry() {
        let handle = seed_session("uuid-close", 14);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::CloseSession {
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            captured,
        );
        assert_eq!(sys_tee_scn_close_ta_session(handle as c_ulong), Ok(()));
        assert_eq!(session_count(), 0);
        server.join();
    }

    #[def_test(user, serial)]
    fn test_close_access_denied_keeps_entry() {
        let handle = seed_session("uuid-close-2", 15);
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::CloseSession {
                    result: TEE_ERROR_ACCESS_DENIED,
                    origin: TEE_ORIGIN_TRUSTED_APP,
                },
            ))],
            Arc::new(Mutex::new(Vec::new())),
        );
        assert_eq!(
            sys_tee_scn_close_ta_session(handle as c_ulong),
            Err(TEE_ERROR_ACCESS_DENIED)
        );
        // §6.6:ACCESS_DENIED 不删表(owner 仍存在,重试语义保持)。
        assert_eq!(session_count(), 1);
        server.join();
    }

    #[def_test(user, serial)]
    fn test_close_item_not_found_removes_entry() {
        let handle = seed_session("uuid-close-3", 16);
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::CloseSession {
                    result: TEE_ERROR_ITEM_NOT_FOUND,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            Arc::new(Mutex::new(Vec::new())),
        );
        assert_eq!(
            sys_tee_scn_close_ta_session(handle as c_ulong),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        assert_eq!(session_count(), 0);
        server.join();
    }

    #[def_test(user, serial)]
    fn test_close_communication_response_deletes_entry_without_retry() {
        let handle = seed_session("uuid-close-comms", 22);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::CloseSession {
                    result: TEE_ERROR_COMMUNICATION,
                    origin: TEE_ORIGIN_COMMS,
                },
            ))],
            Arc::clone(&captured),
        );
        assert_eq!(
            sys_tee_scn_close_ta_session(handle as c_ulong),
            Err(TEE_ERROR_COMMUNICATION)
        );
        assert_eq!(session_count(), 0);
        assert_eq!(
            sys_tee_scn_close_ta_session(handle as c_ulong),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        let (_buf, ptr) = build_user_params([(0, 0, 0); 4]);
        let ret_orig = crate::TestUserValue::<c_uint>::from_value(0).unwrap();
        assert_eq!(
            sys_tee_scn_invoke_ta_command(
                handle as c_ulong,
                u32::MAX as c_ulong,
                1,
                ptr,
                ret_orig.as_user_ptr(),
            ),
            Err(TEE_ERROR_ITEM_NOT_FOUND)
        );
        assert_eq!(ret_orig.read(), TEE_ORIGIN_API);
        server.join();
        assert_eq!(captured.lock().len(), 1);
    }

    #[def_test(user, serial)]
    fn test_close_unreachable_deletes_entry() {
        // 固定路径无监听:通信失败仍要删除表项(§6.6)。
        let handle = seed_session("uuid-close-4", 17);
        assert_eq!(
            sys_tee_scn_close_ta_session(handle as c_ulong),
            Err(TEE_ERROR_COMMUNICATION)
        );
        assert_eq!(session_count(), 0);
        assert!(with_tee_ta_ctx(|ctx| Ok(ctx.needs_owner_release())).unwrap());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_scripted_control_server(
            CONTROL_SOCKET_PATH,
            vec![ScriptedReply::Frame(response_frame(
                ControlResponse::ReleaseOwner {
                    result: TEE_SUCCESS,
                    origin: TEE_ORIGIN_TEE,
                },
            ))],
            Arc::clone(&captured),
        );
        crate::tee::tee_ta_manager::tee_ta_release_sessions().unwrap();
        server.join();
        assert_eq!(captured.lock().len(), 1);
        let frame: ControlRequestFrame =
            crate::tee::control_protocol::decode_control_frame(&captured.lock()[0]).unwrap();
        assert!(matches!(frame.request, ControlRequest::ReleaseOwner));
        assert!(!with_tee_ta_ctx(|ctx| Ok(ctx.needs_owner_release())).unwrap());
    }
