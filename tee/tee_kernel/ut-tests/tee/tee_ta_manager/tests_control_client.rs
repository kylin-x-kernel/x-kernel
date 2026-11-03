// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::String, sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicU64, Ordering};

    use knet::{
        AcceptOptions, SocketAddrEx, SocketOps,
        unix::{StreamTransport, UnixAddr, UnixDomainSocket, UnixSocketType},
    };
    use ksync::Mutex;
    use ktask::KtaskRef;
    use ktime_types::TimeSpan;
    use unittest::{assert_eq, def_test};

    use super::{
        CONTROL_SOCKET_PATH, ControlClientError, ControlDeadline, control_exchange_at,
        control_op_timeout, release_owner_timeout, tee_ta_release_sessions, transfer_exact,
    };
    use crate::tee::{
        control_protocol::{
            CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestFrame, ControlResponse,
            ControlResponseFrame,
        },
        tee_session::{with_tee_ta_ctx, with_tee_ta_ctx_mut},
    };

    static TEST_MONOTONIC_TIME_MILLIS: AtomicU64 = AtomicU64::new(0);

    fn test_monotonic_time() -> ktime_types::MonotonicInstant {
        ktime_types::MonotonicInstant::from_span_since_origin(TimeSpan::from_millis(
            TEST_MONOTONIC_TIME_MILLIS.load(Ordering::Relaxed),
        ))
    }

    /// 每个用例独立的 mock socket 路径(测试内 /tmp 可写;固定控制面路径
    /// 只允许 ReleaseOwner 集成用例使用,避免同 boot 内二次 bind 冲突)。
    fn unique_test_path(name: &str) -> String {
        alloc::format!("/tmp/tee-control-client-test-{name}.sock")
    }

    /// Unix socket 路径节点在 bind 后持久存在(POSIX 语义):重新 bind 同一
    /// 路径前必须先 unlink,否则 AddrInUse。串行用例共用固定控制面路径。
    use super::control_test_support::ensure_socket_node_absent;

    struct SocketNodeGuard(String);

    impl Drop for SocketNodeGuard {
        fn drop(&mut self) {
            ensure_socket_node_absent(&self.0);
        }
    }

    /// 绑定 + listen,返回 listener。测试线程(用户上下文)先完成路径
    /// 绑定,客户端 connect 才能解析到它。
    fn bind_listener(path: &str) -> (UnixDomainSocket, SocketNodeGuard) {
        let socket_node_guard = SocketNodeGuard(String::from(path));
        ensure_socket_node_absent(path);
        let listener = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
        listener
            .bind(SocketAddrEx::Unix(UnixAddr::Path(path.into())))
            .expect("bind mock control listener");
        listener.listen(1).expect("listen mock control listener");
        (listener, socket_node_guard)
    }

    use super::control_test_support::{server_recv_frame, server_send_all};

    /// 在 ktask 中服务一次交换:accept → 读请求帧(存入 captured)→ 执行
    /// 应答脚本。客户端的 recv 阻塞在测试线程,服务端必须并发运行。
    fn spawn_mock_server(
        listener: UnixDomainSocket,
        captured: Arc<Mutex<Vec<u8>>>,
        respond: impl FnOnce(&UnixDomainSocket, Vec<u8>) + Send + 'static,
    ) -> KtaskRef {
        ktask::spawn(move || {
            let Ok(accepted) = listener.accept(AcceptOptions::default()) else {
                return;
            };
            let knet::Socket::Unix(conn) = accepted else {
                return;
            };
            let frame = server_recv_frame(&conn);
            *captured.lock() = frame.clone();
            respond(&conn, frame);
        })
    }

    fn capture_slot() -> Arc<Mutex<Vec<u8>>> {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn open_session_request() -> ControlRequest {
        ControlRequest::OpenSession {
            uuid: alloc::string::String::from("936da01f-9abd-4d9d-80c7-02af85c822a8"),
            connection_method: 0,
            params: Default::default(),
        }
    }

    fn release_owner_success_frame() -> Vec<u8> {
        crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
            version: CONTROL_PROTOCOL_VERSION,
            response: ControlResponse::ReleaseOwner {
                result: 0,
                origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
            },
        })
        .expect("encode release response")
    }

    /// §6.3 基本往返:客户端拿到 per-op 变体;服务端收到的请求帧可被
    /// mirror 解码且 version=1。
    #[def_test(user)]
    fn test_control_exchange_roundtrip() {
        let path = unique_test_path("roundtrip");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let captured = capture_slot();
        let response_frame =
            crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
                version: CONTROL_PROTOCOL_VERSION,
                response: ControlResponse::OpenSession {
                    session_id: 7,
                    result: 0,
                    origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
                },
            })
            .expect("encode open response");
        let server = spawn_mock_server(listener, captured.clone(), move |conn, _| {
            server_send_all(conn, &response_frame);
        });

        let response = control_exchange_at(&path, open_session_request(), control_op_timeout())
            .expect("exchange ok");
        match response {
            ControlResponse::OpenSession {
                session_id, result, ..
            } => {
                assert_eq!(session_id, 7);
                assert_eq!(result, 0);
            }
            other => panic!("unexpected response variant: {other:?}"),
        }

        server.join();
        let request_frame = captured.lock().clone();
        let decoded: ControlRequestFrame =
            crate::tee::control_protocol::decode_control_frame(&request_frame)
                .expect("request frame decodes");
        assert_eq!(decoded.version, CONTROL_PROTOCOL_VERSION);
        assert!(matches!(
            decoded.request,
            ControlRequest::OpenSession { .. }
        ));
    }

    /// §6.3 短读:响应帧分三段写出(前缀劈半 + payload 劈半),客户端仍
    /// 必须拼齐整帧。
    #[def_test(user)]
    fn test_control_exchange_short_reads() {
        let path = unique_test_path("short-reads");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let captured = capture_slot();
        let response_frame =
            crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
                version: CONTROL_PROTOCOL_VERSION,
                response: ControlResponse::CloseSession {
                    result: 0,
                    origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
                },
            })
            .expect("encode close response");
        let server = spawn_mock_server(listener, captured, move |conn, _| {
            server_send_all(conn, &response_frame[..2]);
            server_send_all(conn, &response_frame[2..6]);
            server_send_all(conn, &response_frame[6..]);
        });

        let response = control_exchange_at(
            &path,
            ControlRequest::CloseSession { session_id: 42 },
            control_op_timeout(),
        )
        .expect("chunked response still decodes");
        assert!(matches!(
            response,
            ControlResponse::CloseSession { result: 0, .. }
        ));
        server.join();
    }

    /// §6.3 版本校验:response version=2 → COMMUNICATION + COMMS。
    #[def_test(user)]
    fn test_control_exchange_rejects_wrong_version() {
        let path = unique_test_path("bad-version");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let response_frame =
            crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
                version: CONTROL_PROTOCOL_VERSION + 1,
                response: ControlResponse::OpenSession {
                    session_id: 1,
                    result: 0,
                    origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
                },
            })
            .expect("encode bad version");
        let server = spawn_mock_server(listener, capture_slot(), move |conn, _| {
            server_send_all(conn, &response_frame);
        });

        let err = control_exchange_at(&path, open_session_request(), control_op_timeout())
            .expect_err("wrong version must fail");
        assert_eq!(
            err,
            ControlClientError::Version(CONTROL_PROTOCOL_VERSION + 1)
        );
        // COMMUNICATION + COMMS 映射由 syscall 层行为测试覆盖
        // (close-unreachable / release-unreachable)。
        server.join();
    }

    /// §6.3 变体校验:OpenSession 请求收到 InvokeCommand 响应 → 协议违规。
    #[def_test(user)]
    fn test_control_exchange_rejects_variant_mismatch() {
        let path = unique_test_path("variant-mismatch");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let response_frame =
            crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
                version: CONTROL_PROTOCOL_VERSION,
                response: ControlResponse::InvokeCommand {
                    params: Default::default(),
                    result: 0,
                    origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
                },
            })
            .expect("encode mismatched variant");
        let server = spawn_mock_server(listener, capture_slot(), move |conn, _| {
            server_send_all(conn, &response_frame);
        });

        let err = control_exchange_at(&path, open_session_request(), control_op_timeout())
            .expect_err("variant mismatch must fail");
        assert_eq!(err, ControlClientError::Variant);
        server.join();
    }

    /// §4.2/§6.3 帧上限:长度前缀声明 65,537 → 在分配前拒绝。
    #[def_test(user)]
    fn test_control_exchange_rejects_oversize_prefix() {
        let path = unique_test_path("oversize");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let server = spawn_mock_server(listener, capture_slot(), move |conn, _| {
            // LE u32 = 65_537,不带 payload。
            server_send_all(conn, &(65_537u32).to_le_bytes());
        });

        let err = control_exchange_at(&path, open_session_request(), control_op_timeout())
            .expect_err("oversize prefix must fail");
        assert_eq!(
            err,
            ControlClientError::Frame(crate::tee::control_protocol::ControlFrameError::Oversize(
                65_537
            ))
        );
        server.join();
    }

    /// §6.3 manager 不可达(无监听者):连接失败 → COMMUNICATION。
    #[def_test(user)]
    fn test_control_exchange_unreachable() {
        let path = unique_test_path("unreachable");
        let err = control_exchange_at(&path, open_session_request(), control_op_timeout())
            .expect_err("no listener must fail");
        assert_eq!(err, ControlClientError::Connect);
    }

    /// §6.3 短写:unix stream 的可写水位是 16 KiB(STREAM_WRITABLE_MAX_
    /// OCCUPIED_BYTES),60 KiB 请求帧必然被 send 分片;客户端的整帧写出
    /// 循环必须拼齐,服务端收到的帧可完整解码。
    #[def_test(user)]
    fn test_control_exchange_large_request_frame_survives_short_writes() {
        let path = unique_test_path("short-writes");
        let (listener, _socket_node_guard) = bind_listener(&path);
        let captured = capture_slot();
        let response_frame =
            crate::tee::control_protocol::encode_control_frame(&ControlResponseFrame {
                version: CONTROL_PROTOCOL_VERSION,
                response: ControlResponse::OpenSession {
                    session_id: 3,
                    result: 0,
                    origin: crate::tee::control_protocol::TEE_ORIGIN_TEE,
                },
            })
            .expect("encode open response");
        let server = spawn_mock_server(listener, captured.clone(), move |conn, _| {
            server_send_all(conn, &response_frame);
        });

        // 60 KiB memref data:payload 远超 16 KiB 可写水位,首写必然短写。
        let big_data = alloc::vec![0xA5u8; 60 * 1024];
        let request = ControlRequest::OpenSession {
            uuid: alloc::string::String::from("936da01f-9abd-4d9d-80c7-02af85c822a8"),
            connection_method: 0,
            params: crate::tee::control_protocol::TEE_Parameters(
                crate::tee::control_protocol::TEE_Parameter {
                    param: crate::tee::control_protocol::TEE_Param {
                        data: big_data.clone(),
                        values: Default::default(),
                    },
                    param_type: crate::tee::control_protocol::TEE_ParamType::MemrefInput,
                },
                Default::default(),
                Default::default(),
                Default::default(),
            ),
        };

        let response = control_exchange_at(&path, request, control_op_timeout())
            .expect("large frame exchange ok");
        assert!(matches!(
            response,
            ControlResponse::OpenSession { session_id: 3, .. }
        ));
        server.join();

        // 服务端收到的帧与发出的请求逐字节一致(短写循环没有丢字节)。
        let request_frame = captured.lock().clone();
        let decoded: ControlRequestFrame =
            crate::tee::control_protocol::decode_control_frame(&request_frame)
                .expect("large request frame decodes");
        match decoded.request {
            ControlRequest::OpenSession { params, .. } => {
                assert_eq!(params.0.param.data, big_data);
            }
            other => panic!("unexpected request variant: {other:?}"),
        }
    }

    /// §6.3/§9.3 接收超时:listener 不 accept(连接入队即成功),客户端
    /// 在超时内返回 Recv 失败而不是永久阻塞。缩短超时避免测试拖慢 QEMU。
    #[def_test(user)]
    fn test_control_exchange_recv_timeout_is_bounded() {
        let path = unique_test_path("recv-timeout");
        // 只 bind+listen,从不 accept:连接排队成功,recv 无数据可读。
        let (_listener, _socket_node_guard) = bind_listener(&path);
        let short = TimeSpan::from_millis(300);

        let err = control_exchange_at(&path, open_session_request(), short)
            .expect_err("no response must time out");
        assert_eq!(err, ControlClientError::Recv);
    }

    #[def_test]
    fn test_partial_io_shares_one_absolute_deadline() {
        let timeout = release_owner_timeout();
        assert_eq!(timeout, TimeSpan::from_millis(1000));
        TEST_MONOTONIC_TIME_MILLIS.store(10, Ordering::Relaxed);
        let deadline = ControlDeadline::after_with_clock(timeout, test_monotonic_time)
            .expect("test deadline should fit");

        let mut send_timeouts = Vec::new();
        let mut send_offsets = Vec::new();
        let mut send_calls = 0;
        assert_eq!(
            transfer_exact(
                3,
                ControlClientError::Send,
                ControlClientError::Send,
                || {
                    send_timeouts.push(deadline.remaining()?);
                    Ok(())
                },
                |offset| {
                    send_offsets.push(offset);
                    send_calls += 1;
                    TEST_MONOTONIC_TIME_MILLIS.store(
                        match offset {
                            0 => 260,
                            1 => 710,
                            _ => 710,
                        },
                        Ordering::Relaxed,
                    );
                    Ok(1)
                },
            ),
            Ok(())
        );
        assert_eq!(send_calls, 3);
        assert_eq!(send_offsets, [0, 1, 2]);
        assert_eq!(
            send_timeouts,
            [
                timeout,
                TimeSpan::from_millis(750),
                TimeSpan::from_millis(300)
            ]
        );

        let mut receive_timeouts = Vec::new();
        let mut receive_offsets = Vec::new();
        let mut receive_calls = 0;
        assert_eq!(
            transfer_exact(
                3,
                ControlClientError::Eof,
                ControlClientError::Recv,
                || {
                    receive_timeouts.push(deadline.remaining()?);
                    Ok(())
                },
                |offset| {
                    receive_offsets.push(offset);
                    receive_calls += 1;
                    TEST_MONOTONIC_TIME_MILLIS.store(1010, Ordering::Relaxed);
                    Ok(1)
                },
            ),
            Err(ControlClientError::Deadline)
        );
        assert_eq!(receive_calls, 1);
        assert_eq!(receive_offsets, [0]);
        assert_eq!(receive_timeouts, [TimeSpan::from_millis(300)]);
        TEST_MONOTONIC_TIME_MILLIS.store(0, Ordering::Relaxed);
    }

    /// §8.2 无 session 的进程不触碰控制面 socket:未启动任何监听者,
    /// release 也必须直接 Ok。
    #[def_test(user, serial)]
    fn test_release_sessions_without_sessions_skips_socket() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            Ok(())
        })
        .expect("clear ctx sessions");

        // 固定路径上没有 listener;若实现误发起连接将得到 Connect 失败。
        assert_eq!(tee_ta_release_sessions(), Ok(()));
    }

    /// §8.2 ReleaseOwner 集成:固定控制面路径、1 秒预算、成功响应 → 清空
    /// 本地表。串行执行(同一 boot 内固定路径只能 bind 一次)。
    #[def_test(user, serial)]
    fn test_release_sessions_clears_table_on_success() {
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            ctx.insert_session(alloc::string::String::from("uuid-a"), 5);
            ctx.insert_session(alloc::string::String::from("uuid-b"), 6);
            Ok(())
        })
        .expect("seed sessions");

        let (listener, _socket_node_guard) = bind_listener(CONTROL_SOCKET_PATH);
        let response_frame = release_owner_success_frame();
        let server = spawn_mock_server(listener, capture_slot(), move |conn, _| {
            server_send_all(conn, &response_frame);
        });

        assert_eq!(tee_ta_release_sessions(), Ok(()));
        server.join();

        let sessions_left = with_tee_ta_ctx(|ctx| Ok(ctx.has_open_sessions())).expect("read ctx");
        assert!(!sessions_left, "local table must be empty after release");
    }

    /// §8.2 manager 不可达:release 返回 COMMUNICATION 但本地表仍被清空
    /// (不重试、退出不被阻塞)。
    #[def_test(user, serial)]
    fn test_release_sessions_clears_table_when_unreachable() {
        // 使用与「无 session」用例不同的进程状态:先种入 session,但不在
        // 固定路径上放 listener(前一个串行用例的 listener 已随作用域关闭,
        // socket 节点仍在但无绑定 → ConnectionRefused)。
        with_tee_ta_ctx_mut(|ctx| {
            ctx.clear_sessions();
            ctx.insert_session(alloc::string::String::from("uuid-a"), 9);
            Ok(())
        })
        .expect("seed sessions");

        let result = tee_ta_release_sessions();
        assert_eq!(result, Err(tee_raw_sys::TEE_ERROR_COMMUNICATION));

        let sessions_left = with_tee_ta_ctx(|ctx| Ok(ctx.has_open_sessions())).expect("read ctx");
        assert!(
            !sessions_left,
            "unreachable manager must still clear the local table"
        );
    }

#[def_test(user)]
fn test_control_send_continues_after_task_interruption() {
    let (sender, receiver) = StreamTransport::new_pair(knet::unix::current_peer_credentials());
    let sender = UnixDomainSocket::new(sender, UnixSocketType::Stream);
    let receiver = UnixDomainSocket::new(receiver, UnixSocketType::Stream);
    let deadline = ControlDeadline::after(control_op_timeout()).expect("deadline fits");
    ktask::current().interrupt();
    let result = super::send_frame(&sender, b"one request", deadline);
    assert_eq!(result, Ok(()));
    let mut received = [0; 11];
    super::recv_exact(&receiver, &mut received, deadline).expect("one request sent");
    assert_eq!(&received, b"one request");
}

#[def_test(user)]
fn test_control_receive_continues_after_task_interruption() {
    let (sender, receiver) = StreamTransport::new_pair(knet::unix::current_peer_credentials());
    let sender = UnixDomainSocket::new(sender, UnixSocketType::Stream);
    let receiver = UnixDomainSocket::new(receiver, UnixSocketType::Stream);
    let deadline = ControlDeadline::after(control_op_timeout()).expect("deadline fits");
    super::send_frame(&sender, b"one response", deadline).expect("response ready");
    ktask::current().interrupt();
    let mut received = [0; 12];
    let result = super::recv_exact(&receiver, &mut received, deadline);
    assert_eq!(result, Ok(()));
    assert_eq!(&received, b"one response");
}

#[def_test(serial)]
fn test_interrupted_io_preserves_offset_and_absolute_deadline() {
    use kerrno::KError;

    TEST_MONOTONIC_TIME_MILLIS.store(0, Ordering::Relaxed);
    let deadline = ControlDeadline::after_with_clock(release_owner_timeout(), test_monotonic_time)
        .expect("deadline fits");
    let mut offsets = Vec::new();
    let mut timeouts = Vec::new();
    let mut attempts = 0;
    let result = transfer_exact(
        3,
        ControlClientError::Send,
        ControlClientError::Send,
        || {
            timeouts.push(deadline.remaining()?);
            Ok(())
        },
        |offset| {
            offsets.push(offset);
            attempts += 1;
            TEST_MONOTONIC_TIME_MILLIS.store(attempts * 100, Ordering::Relaxed);
            match attempts {
                1 => Ok(1),
                2 => Err(KError::Interrupted),
                _ => Ok(2),
            }
        },
    );
    assert_eq!(result, Ok(()));
    assert_eq!(offsets, [0, 1, 1]);
    assert_eq!(
        timeouts,
        [
            TimeSpan::from_millis(1000),
            TimeSpan::from_millis(900),
            TimeSpan::from_millis(800)
        ]
    );

    let mut attempts = 0;
    let result = transfer_exact(
        1,
        ControlClientError::Eof,
        ControlClientError::Recv,
        || deadline.remaining().map(|_| ()),
        |_| {
            attempts += 1;
            TEST_MONOTONIC_TIME_MILLIS.store(300 + attempts * 400, Ordering::Relaxed);
            Err(KError::Interrupted)
        },
    );
    assert_eq!(result, Err(ControlClientError::Deadline));
    assert_eq!(attempts, 2);
    TEST_MONOTONIC_TIME_MILLIS.store(0, Ordering::Relaxed);
}

#[def_test]
fn test_terminal_io_failure_is_not_retried() {
    let mut attempts = 0;
    let result = transfer_exact(
        1,
        ControlClientError::Send,
        ControlClientError::Send,
        || Ok(()),
        |_| {
            attempts += 1;
            Err(kerrno::KError::BrokenPipe)
        },
    );
    assert_eq!(result, Err(ControlClientError::Send));
    assert_eq!(attempts, 1);
}
