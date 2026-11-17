// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x-kernel 侧的控制面 wire mirror 与 LE framing（plan 004 §4）。
//!
//! 这是 manager 端 `xtee-apps/gp/vsock-manager/src/control_protocol.rs` 的
//! **独立镜像**：两端不共享 crate（内核 workspace 不引入 teec-protocol 依赖），
//! 一致性只由两端各自提交的同一组 F1–F9 固定 golden 常量保证。改任何字段或
//! variant 顺序前必须先改设计，再同步两端 golden，两侧任一测试变红即视为协议
//! 漂移，不允许只改一端。
//!
//! 与既有内部协议 [`protocal`](crate::tee::protocal) 的边界：本模块只描述
//! x-kernel ↔ vsock-manager 控制面，framing 为 4 字节 **little-endian** u32
//! 长度 + postcard payload（内部协议是 native-endian 前缀，两套不可混用）。
//! socket I/O、per-session 超时和 TEE 错误码映射属于控制面客户端（ticket 06，
//! [`tee_ta_manager`](crate::tee::tee_ta_manager)），不在本模块内。
//!
//! 序列化只用 serde derive：`TEE_ParamType` 的 wire 编码是**声明顺序的
//! variant index**（`MemrefInput` → 4），不是 GP 值（5）。discriminant 是 GP
//! 值，供 utee 参数转换使用；禁止引入自定义 serializer 把 GP 值写上 wire。

// 类型名（`TEE_Parameters` 等）与 teec-protocol、manager 端镜像逐字对齐是
// plan 004 §4.1 的硬约束：两端字段/variant 顺序靠「同名同序」人工比对。
#![expect(
    non_camel_case_types,
    reason = "CG-024-lint-exceptions: non_camel_case_types: the control-plane wire mirror exposes \
              the external teec-protocol type names verbatim"
)]
#![expect(
    clippy::upper_case_acronyms,
    reason = "CG-032-camel-case-types: the control-plane wire mirror retains the external \
              teec-protocol type names"
)]
// 本 ticket 只交付 mirror + golden（交付边界：内核 `tee/mod.rs` 仅新增一行
// 注册）。控制面客户端在 ticket 06 接线，此前这些类型只有测试引用，属预期
// 中间态；接线后本 expect 不再触发时编译器会指出，届时移除。
#![expect(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: mirror-first delivery per ticket 03; the ticket \
              06 control-plane client wires it"
)]

use alloc::{string::String, vec::Vec};

use postcard::{from_bytes, to_allocvec};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// 控制面协议版本（plan 004 §3 冻结：`CONTROL_PROTOCOL_VERSION = 1`）。
pub(super) const CONTROL_PROTOCOL_VERSION: u16 = 1;

/// 单帧控制面 payload 上限：读侧必须先经 [`control_frame_payload_len`] 校验
/// 再分配缓冲（plan 004 §4.2）。
pub(super) const MAX_CONTROL_PAYLOAD: usize = 65_536;

// GP origin 常量（`tee_raw_sys::tee_api_defines` 已有同名 pub 常量；这里按
// mirror 自洽原则独立定义同值，wire 断言只依赖本模块，避免向 tee_raw_sys
// 建立新的可见性耦合）。
pub(super) const TEE_ORIGIN_API: u32 = 0x0000_0001;
pub(super) const TEE_ORIGIN_COMMS: u32 = 0x0000_0002;
pub(super) const TEE_ORIGIN_TEE: u32 = 0x0000_0003;
pub(super) const TEE_ORIGIN_TRUSTED_APP: u32 = 0x0000_0004;

/// 控制面帧编解码错误。
///
/// ticket 06 的客户端把所有变体统一映射为 `TEE_ERROR_COMMUNICATION +
/// TEE_ORIGIN_COMMS`（§6.3/§6.7），变体区分只服务诊断与测试。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ControlFrameError {
    /// postcard 序列化失败。
    Serialize,
    /// postcard 反序列化失败。
    Deserialize,
    /// payload 超过 [`MAX_CONTROL_PAYLOAD`]。
    Oversize(usize),
    /// 输入不是完整帧（前缀长度与 payload 不一致）。
    Truncated,
}

/// 控制面请求帧（plan 004 §4.1）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) struct ControlRequestFrame {
    pub(super) version: u16,
    pub(super) request: ControlRequest,
}

/// 控制面响应帧（plan 004 §4.1）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) struct ControlResponseFrame {
    pub(super) version: u16,
    pub(super) response: ControlResponse,
}

/// 控制面请求变体。
///
/// 与内部转发协议的差异：没有 `ca_auth_info`（控制面强制 `None`，§3）与
/// `RequestCancellation`（控制面不透传），新增 `ReleaseOwner`（进程退出清理）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) enum ControlRequest {
    OpenSession {
        uuid: String,
        connection_method: u32,
        params: TEE_Parameters,
    },
    CloseSession {
        session_id: u32,
    },
    InvokeCommand {
        session_id: u32,
        cmd_id: u32,
        params: TEE_Parameters,
    },
    ReleaseOwner,
    /// Limited cooperative Invoke timeout extension; old v1 variants stay fixed.
    InvokeCommandWithTimeout {
        session_id: u32,
        cmd_id: u32,
        params: TEE_Parameters,
        deadline_ns: u64,
    },
}

/// 控制面响应变体（plan 004 §4.1）。
///
/// [`ControlResponse::Error`] 只用于「已经无法安全确定具体操作」的失败：peer
/// 身份获取失败、版本不支持、未知请求变体。已成功解码的请求无论成败都必须返回
/// 对应的 per-operation 变体，不能把业务错误改包成 `Error`。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(super) enum ControlResponse {
    Error {
        result: u32,
        origin: u32,
    },
    OpenSession {
        session_id: u32,
        result: u32,
        origin: u32,
    },
    CloseSession {
        result: u32,
        origin: u32,
    },
    InvokeCommand {
        params: TEE_Parameters,
        result: u32,
        origin: u32,
    },
    ReleaseOwner {
        result: u32,
        origin: u32,
    },
}

// ---------------------------------------------------------------------------
// 参数镜像：字段顺序与 teec-protocol 完全一致（plan 004 §4.1）。
// ---------------------------------------------------------------------------

/// 四个 TEE 参数的容器。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub(super) struct TEE_Parameters(
    pub(super) TEE_Parameter,
    pub(super) TEE_Parameter,
    pub(super) TEE_Parameter,
    pub(super) TEE_Parameter,
);

/// 单个 TEE 参数：`param` 携带数据，`param_type` 指示读写方向。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub(super) struct TEE_Parameter {
    pub(super) param: TEE_Param,
    pub(super) param_type: TEE_ParamType,
}

/// 参数数据载体：`data`（memref 缓冲）与 `values`（value 对）。
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub(super) struct TEE_Param {
    pub(super) data: Vec<u8>,
    pub(super) values: TEE_Value,
}

/// VALUE 参数的 (a, b) 对。
///
/// `MEMREF_OUTPUT` 约定复用 `values.a` 传递容量（teec-protocol 现行约定，§7.2）。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Default)]
pub(super) struct TEE_Value {
    pub(super) a: u32,
    pub(super) b: u32,
}

/// 参数类型。wire 编码 = 声明顺序 variant index；discriminant = GP 值。
///
/// `MemrefInput` 的 wire index 是 **4**，GP 值是 5 —— 两者并存，serde derive
/// 只用声明序，禁止引入自定义 serializer 把 GP 值写上 wire（§4.1）。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum TEE_ParamType {
    #[default]
    None         = 0,
    ValueInput   = 1,
    ValueOutput  = 2,
    ValueInout   = 3,
    MemrefInput  = 5,
    MemrefOutput = 6,
    MemrefInout  = 7,
}

// ---------------------------------------------------------------------------
// LE framing（plan 004 §4.2；socket I/O 在 ticket 06 的客户端）
// ---------------------------------------------------------------------------

/// 从 4 字节 LE 长度前缀解析 payload 长度。
///
/// 读侧必须先调用本函数校验，再分配接收缓冲：65,536 以上的长度在这里拒绝，
/// 不会进入任何分配路径（§4.2）。返回 `Ok(len)` 时 `len ≤ MAX_CONTROL_PAYLOAD`。
pub(super) fn control_frame_payload_len(prefix: [u8; 4]) -> Result<usize, ControlFrameError> {
    let len = u32::from_le_bytes(prefix) as usize;
    if len > MAX_CONTROL_PAYLOAD {
        return Err(ControlFrameError::Oversize(len));
    }
    Ok(len)
}

/// 序列化一帧并加上 LE 长度前缀；payload 超限在返回前拒绝。
///
/// 返回值 = `4 字节 LE 长度 + payload`，客户端按 send_all 语义写出。
pub(super) fn encode_control_frame<T: Serialize>(frame: &T) -> Result<Vec<u8>, ControlFrameError> {
    let payload = to_allocvec(frame).map_err(|_| ControlFrameError::Serialize)?;
    if payload.len() > MAX_CONTROL_PAYLOAD {
        return Err(ControlFrameError::Oversize(payload.len()));
    }
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// 解码一个完整帧（`4 字节 LE 长度 + payload`，由客户端拼齐后传入）。
///
/// 依次校验：前缀长度不超过上限、与实际 payload 长度一致、postcard 可解码。
/// `version` 校验留给调用方：请求与响应两个方向都要查，而泛型 `T` 不暴露该
/// 字段。
pub(super) fn decode_control_frame<T: DeserializeOwned>(
    framed: &[u8],
) -> Result<T, ControlFrameError> {
    if framed.len() < 4 {
        return Err(ControlFrameError::Truncated);
    }
    let mut prefix = [0u8; 4];
    prefix.copy_from_slice(&framed[..4]);
    let len = control_frame_payload_len(prefix)?;
    if framed.len() - 4 != len {
        return Err(ControlFrameError::Truncated);
    }
    from_bytes(&framed[4..]).map_err(|_| ControlFrameError::Deserialize)
}

unittest::path_mod_test!(
    tests_control_protocol,
    "../../ut-tests/tee/control_protocol/tests.rs"
);
