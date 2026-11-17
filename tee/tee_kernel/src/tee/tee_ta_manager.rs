// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Inter-TA 会话的 manager 控制面客户端(plan 004 §6.3)。
//!
//! 每个操作新建一条 Unix 连接:发送一帧请求、读取一帧 response、连接随即
//! 关闭(§6.3)。不复用连接,因此无需把 socket 存进 `TeeTaCtx`,也没有多
//! 线程共享连接的 response 串线问题。所有传输/编解码失败统一映射
//! `TEE_ERROR_COMMUNICATION + TEE_ORIGIN_COMMS`,不重放请求。被信号中断的
//! 底层读写从当前字节偏移继续,仍消耗同一个绝对 deadline。
//!
//! Panic 通知是唯一残留的 legacy 直连路径,见 [`protocal`]。

use alloc::{vec, vec::Vec};

use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use knet::{
    ConnectOptions, RecvOptions, SendOptions, SocketAddrEx, SocketOps,
    options::{Configurable, SetSocketOption},
    unix::{StreamTransport, UnixAddr, UnixDomainSocket, UnixSocketType},
};
use ktime_types::{MonotonicInstant, TimeSpan};
use tee_raw_sys::{TEE_ERROR_COMMUNICATION, TEE_SUCCESS};

use crate::tee::{
    TeeResult,
    control_protocol::{
        self, CONTROL_PROTOCOL_VERSION, ControlRequest, ControlRequestFrame, ControlResponse,
        ControlResponseFrame,
    },
    tee_session::{with_tee_ta_ctx, with_tee_ta_ctx_mut},
};

// ---------------------------------------------------------------------------
// 控制面客户端(§6.3/§6.7)
// ---------------------------------------------------------------------------

/// manager 控制面 socket 的固定地址(§6.3)。
pub(crate) const CONTROL_SOCKET_PATH: &str = "/tmp/tee-manager-control.sock";

/// 普通 Open/Invoke/Close 操作的总 socket 预算(§6.3:5 秒)。
pub(super) fn control_op_timeout() -> TimeSpan {
    TimeSpan::from_secs(5)
}

/// `ReleaseOwner` 的 1 秒预算(§8.2:与 x-kernel 退出路径共享的同一上界)。
fn release_owner_timeout() -> TimeSpan {
    TimeSpan::from_millis(1000)
}

/// 控制面客户端失败类别。
///
/// 全部变体统一映射 `TEE_ERROR_COMMUNICATION + TEE_ORIGIN_COMMS`(§6.3/
/// §6.7);变体区分只服务诊断与测试断言。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ControlClientError {
    /// socket 超时选项设置失败(内核资源不足)。
    SetOption,
    /// 连接 manager 控制面失败(无监听、拒绝或排队超时)。
    Connect,
    /// 帧写出失败或写入零字节。
    Send,
    /// 对端在帧完整前关闭连接。
    Eof,
    /// 帧读入失败(含接收超时)。
    Recv,
    /// 编解码或长度前缀违规。
    Frame(control_protocol::ControlFrameError),
    /// response 的 version 不是 [`CONTROL_PROTOCOL_VERSION`]。
    Version(u16),
    /// response 变体与请求操作不匹配(§6.3「严格校验」)。
    Variant,
    /// 单次控制面交换的绝对 deadline 已耗尽。
    Deadline,
}

#[derive(Clone, Copy)]
struct ControlDeadline {
    expires_at: MonotonicInstant,
    monotonic_clock: fn() -> MonotonicInstant,
}

impl ControlDeadline {
    fn after(timeout: TimeSpan) -> Result<Self, ControlClientError> {
        Self::after_with_clock(timeout, monotonic_time)
    }

    fn after_with_clock(
        timeout: TimeSpan,
        monotonic_clock: fn() -> MonotonicInstant,
    ) -> Result<Self, ControlClientError> {
        monotonic_clock()
            .checked_add(timeout)
            .map(|expires_at| Self {
                expires_at,
                monotonic_clock,
            })
            .ok_or(ControlClientError::Deadline)
    }

    fn remaining(self) -> Result<TimeSpan, ControlClientError> {
        let remaining = self
            .expires_at
            .saturating_duration_since((self.monotonic_clock)());
        if remaining.is_zero() {
            Err(ControlClientError::Deadline)
        } else {
            Ok(remaining)
        }
    }
}

/// 在 `path` 上执行一次「连接 → 发一帧 → 收一帧」往返。
///
/// `path` 由调用方传入:生产路径恒为 [`CONTROL_SOCKET_PATH`];测试注入
/// 独立路径以隔离用例(§6.3 固定地址,不提供其他生产入口)。
pub(super) fn control_exchange_at(
    path: &str,
    request: ControlRequest,
    timeout: TimeSpan,
) -> Result<ControlResponse, ControlClientError> {
    let deadline = ControlDeadline::after(timeout)?;
    // §6.3「严格校验 response variant 与请求操作匹配」:manager 已解码请求
    // 后必须应答 per-op 变体;只有无法确定操作时才允许通用 Error。请求
    // 的操作类别在构帧(move 发生前)先行提取。
    let expected_op = control_request_op(&request);
    let frame = control_protocol::encode_control_frame(&ControlRequestFrame {
        version: CONTROL_PROTOCOL_VERSION,
        request,
    })
    .map_err(ControlClientError::Frame)?;
    let socket = connect_control(path, deadline)?;
    send_frame(&socket, &frame, deadline)?;
    let framed = recv_frame(&socket, deadline)?;
    let decoded: ControlResponseFrame =
        control_protocol::decode_control_frame(&framed).map_err(ControlClientError::Frame)?;
    if decoded.version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlClientError::Version(decoded.version));
    }
    match control_response_op(&decoded.response) {
        // Error 变体对任何操作合法(§4.1:仅用于无法确定具体操作)。
        None => {}
        Some(actual_op) if actual_op != expected_op => {
            return Err(ControlClientError::Variant);
        }
        Some(_) => {}
    }
    Ok(decoded.response)
}

/// 请求/响应的操作类别(变体校验用;顺序与 wire 声明无关)。
///
/// 单一枚举同时服务两个方向的映射:请求侧返回类别本身,响应侧返回
/// `Option<ControlOp>`(`Error` 变体 → `None`,对任何请求合法)。
/// 编译器强制两个 match 穷尽全部变体,类型化相等比较杜绝两表漂移。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlOp {
    OpenSession,
    CloseSession,
    InvokeCommand,
    ReleaseOwner,
}

fn control_request_op(request: &ControlRequest) -> ControlOp {
    match request {
        ControlRequest::OpenSession { .. } => ControlOp::OpenSession,
        ControlRequest::CloseSession { .. } => ControlOp::CloseSession,
        ControlRequest::InvokeCommand { .. } | ControlRequest::InvokeCommandWithTimeout { .. } => {
            ControlOp::InvokeCommand
        }
        ControlRequest::ReleaseOwner => ControlOp::ReleaseOwner,
    }
}

fn control_response_op(response: &ControlResponse) -> Option<ControlOp> {
    match response {
        ControlResponse::Error { .. } => None,
        ControlResponse::OpenSession { .. } => Some(ControlOp::OpenSession),
        ControlResponse::CloseSession { .. } => Some(ControlOp::CloseSession),
        ControlResponse::InvokeCommand { .. } => Some(ControlOp::InvokeCommand),
        ControlResponse::ReleaseOwner { .. } => Some(ControlOp::ReleaseOwner),
    }
}

/// 连接 manager 控制面。两个 socket 超时都在 `connect` 之前设置:Unix
/// stream 的 connect 排队同样受 send timeout 约束(§6.3)。
fn connect_control(
    path: &str,
    deadline: ControlDeadline,
) -> Result<UnixDomainSocket, ControlClientError> {
    let socket = UnixDomainSocket::new(StreamTransport::new(), UnixSocketType::Stream);
    set_socket_timeout(&socket, deadline, SocketTimeoutKind::Send)?;
    set_socket_timeout(&socket, deadline, SocketTimeoutKind::Receive)?;
    socket
        .connect(
            SocketAddrEx::Unix(UnixAddr::Path(path.into())),
            ConnectOptions::default(),
        )
        .map_err(|_| ControlClientError::Connect)?;
    Ok(socket)
}

#[derive(Clone, Copy)]
enum SocketTimeoutKind {
    Send,
    Receive,
}

/// Refreshes the timeout before each blocking socket operation so loops over
/// short writes and reads consume one exchange-wide deadline.
fn set_socket_timeout(
    socket: &UnixDomainSocket,
    deadline: ControlDeadline,
    kind: SocketTimeoutKind,
) -> Result<(), ControlClientError> {
    let remaining = deadline.remaining()?;
    let option = match kind {
        SocketTimeoutKind::Send => SetSocketOption::SendTimeout(&remaining),
        SocketTimeoutKind::Receive => SetSocketOption::ReceiveTimeout(&remaining),
    };
    socket
        .set_option(option)
        .map_err(|_| ControlClientError::SetOption)
}

/// 整帧写出,处理短写:send 返回 0 视为失败而不是空转。
fn send_frame(
    socket: &UnixDomainSocket,
    frame: &[u8],
    deadline: ControlDeadline,
) -> Result<(), ControlClientError> {
    transfer_exact(
        frame.len(),
        ControlClientError::Send,
        ControlClientError::Send,
        || set_socket_timeout(socket, deadline, SocketTimeoutKind::Send),
        |offset| socket.send(&frame[offset..], SendOptions::default()),
    )
}

/// 读取完整一帧(4 字节 LE 长度前缀 + payload),处理短读。
///
/// 返回「前缀 + payload」的完整帧字节,供 mirror 的
/// [`control_protocol::decode_control_frame`] 解码。
fn recv_frame(
    socket: &UnixDomainSocket,
    deadline: ControlDeadline,
) -> Result<Vec<u8>, ControlClientError> {
    let mut prefix = [0u8; 4];
    recv_exact(socket, &mut prefix, deadline)?;
    // §4.2/§6.3:长度前缀在分配前校验,65,536 以上在这里拒绝。
    let len =
        control_protocol::control_frame_payload_len(prefix).map_err(ControlClientError::Frame)?;
    let mut framed = Vec::with_capacity(4 + len);
    framed.extend_from_slice(&prefix);
    let mut payload = vec![0u8; len];
    recv_exact(socket, &mut payload, deadline)?;
    framed.extend_from_slice(&payload);
    Ok(framed)
}

/// 精确读满 `buf`,处理短读;对端提前关闭返回 [`ControlClientError::Eof`]。
fn recv_exact(
    socket: &UnixDomainSocket,
    buf: &mut [u8],
    deadline: ControlDeadline,
) -> Result<(), ControlClientError> {
    transfer_exact(
        buf.len(),
        ControlClientError::Eof,
        ControlClientError::Recv,
        || set_socket_timeout(socket, deadline, SocketTimeoutKind::Receive),
        |offset| socket.recv(&mut buf[offset..], RecvOptions::default()),
    )
}

fn transfer_exact(
    len: usize,
    zero_progress_error: ControlClientError,
    io_error: ControlClientError,
    mut set_timeout: impl FnMut() -> Result<(), ControlClientError>,
    mut transfer: impl FnMut(usize) -> KResult<usize>,
) -> Result<(), ControlClientError> {
    let mut transferred = 0;
    while transferred < len {
        set_timeout()?;
        let count = match transfer(transferred) {
            Ok(count) => count,
            // EINTR reports no progress for this syscall. Keep the byte offset
            // and refresh the remaining budget before trying it again.
            Err(KError::Interrupted) => continue,
            Err(_) => return Err(io_error),
        };
        if count == 0 {
            return Err(zero_progress_error);
        }
        transferred += count;
    }
    Ok(())
}

/// 进程退出清理入口(plan 004 §8.2;由 POSIX 的 `exit_tee` 阶段调用)。
///
/// 要求当前退出用户线程的可睡眠上下文,且不持有 membership 锁。调用时
/// 全局 TID 已取消发布,但进程 runtime 和文件系统上下文仍有效;这些状态
/// 通过当前线程访问,不依赖 TID 目录。必须在 `exit_mm`、TEE runtime 清理、
/// `exit_fs`、namespace 清理和 `complete_process_exit` 之前完成。
///
/// 语义:最多 1 秒 best-effort 的 `ReleaseOwner` socket I/O + 清空本地
/// session 表。没有 open session 且无未确认 Close 的进程不触碰 socket。
/// 响应中的单个 close 失败按设计忽略(仅日志);manager 不可达或协议
/// 失败返回 `Err(TEE_ERROR_COMMUNICATION)` 供调用方记录 —— 本地表仍被
/// 清空,不重试、不阻塞进程退出(与发送共享同一 1 秒预算)。
pub fn tee_ta_release_sessions() -> TeeResult {
    let has_sessions = with_tee_ta_ctx(|ctx| Ok(ctx.needs_owner_release()))?;
    if !has_sessions {
        return Ok(());
    }

    let exchange = control_exchange_at(
        CONTROL_SOCKET_PATH,
        ControlRequest::ReleaseOwner,
        release_owner_timeout(),
    );
    let outcome = match exchange {
        Ok(ControlResponse::ReleaseOwner { result, .. }) => {
            if result != TEE_SUCCESS {
                // §8.2 步骤 4:单个 close 失败不中断退出,只留日志。
                warn!("ReleaseOwner finished with partial close failures: result={result:#010x}");
            }
            Ok(())
        }
        // 已知操作必须应答 per-op 变体;变体错配按协议违规处理。
        Ok(_) => Err(TEE_ERROR_COMMUNICATION),
        Err(err) => {
            // manager 不可达:1 秒内失败,不重试(§8.2 步骤 6)。错误
            // 类别固定 COMMUNICATION(§6.3);origin 恒 COMMS 由调用方
            // 按 §6.7 表第 2 行决定,无需从错误变体再推导。
            warn!("ReleaseOwner transport failed: {:?}", err);
            Err(TEE_ERROR_COMMUNICATION)
        }
    };

    // §8.2 步骤 5:无论响应如何都清空本地表。
    with_tee_ta_ctx_mut(|ctx| {
        ctx.clear_sessions();
        Ok(())
    })?;
    outcome
}

/// 控制面客户端与 inter-TA syscall 单测共用的内核内 mock 服务端工具。
///
/// 仅存在于 unittest 构建;归入 tee_ta_manager 而非新文件,遵守 §6.1
/// 文件清单(生产代码零新增文件)。
#[cfg(unittest)]
// CG-021 例外:与 mod_test 生成的豁免一致 —— 仅单测可达的支持代码允许
// 在基础设施故障处 panic/expect(失败即测试失败)。
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: unittest-only support code for control-plane \
              mock servers"
)]
pub(super) mod control_test_support {
    use alloc::vec::Vec;

    use knet::{RecvOptions, SendOptions, SocketOps, unix::UnixDomainSocket};

    /// Unix socket 路径节点在 bind 后持久存在(POSIX 语义):重新 bind
    /// 同一路径前必须先 unlink,否则 AddrInUse。串行用例共用固定控制面
    /// 路径,每例开始前清掉上一例留下的节点;NotFound = 本来就不存在。
    pub(crate) fn ensure_socket_node_absent(path: &str) {
        let cred = kprocess::current_cred();
        let filename = kvfs::Filename::new(path);
        let fs = kprocess::current_fs_context();
        let fs = fs.lock();
        if let Err(err) = filename.unlink_at(fs.root(), fs.pwd(), &cred) {
            assert!(
                matches!(err, kvfs::VfsError::NotFound),
                "unexpected unlink error for {path}: {err:?}"
            );
        }
    }

    /// 读取完整一帧(server 侧,镜像客户端语义):前缀 + payload。
    pub(crate) fn server_recv_frame(conn: &UnixDomainSocket) -> Vec<u8> {
        let mut prefix = [0u8; 4];
        server_recv_exact(conn, &mut prefix);
        let len = u32::from_le_bytes(prefix) as usize;
        let mut framed = Vec::with_capacity(4 + len);
        framed.extend_from_slice(&prefix);
        let mut payload = alloc::vec![0u8; len];
        server_recv_exact(conn, &mut payload);
        framed.extend_from_slice(&payload);
        framed
    }

    fn server_recv_exact(conn: &UnixDomainSocket, buf: &mut [u8]) {
        let mut filled = 0;
        while filled < buf.len() {
            let n = conn
                .recv(&mut buf[filled..], RecvOptions::default())
                .expect("server recv");
            assert!(n > 0, "client closed before sending a whole frame");
            filled += n;
        }
    }

    /// 整帧写出(server 侧);零字节写出视为失败。
    pub(crate) fn server_send_all(conn: &UnixDomainSocket, mut data: &[u8]) {
        while !data.is_empty() {
            let n = conn
                .send(data, SendOptions::default())
                .expect("server send");
            assert!(n > 0, "server send wrote zero bytes");
            data = &data[n..];
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/tee/tee_ta_manager/tests.rs");

unittest::path_mod_test!(
    tests_control_client,
    "../../ut-tests/tee/tee_ta_manager/tests_control_client.rs"
);
