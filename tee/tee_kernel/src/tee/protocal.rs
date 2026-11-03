// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! legacy Panic 通知 wire(ticket 06:inter-TA 会话已迁至控制面)。
//!
//! inter-TA 的 Open/Invoke/Close 不再使用本模块:vsock-manager 控制面
//! ([`control_protocol`](crate::tee::control_protocol)) 是唯一会话通道。
//! 这里只剩 TA Panic 通知:内核直接向 `/tmp/{uuid}.sock` 发送一帧
//! legacy `TeeRequest::Panic` 等价字节,TA runtime 侧镜像仍按旧 wire
//! 解码。
//!
//! # 字节兼容契约
//!
//! 旧 `TeeRequest::Panic { panic_code }` 的 postcard 编码是
//! `[varint(变体序号=4), varint(panic_code)]`;[`LegacyPanicFrame`]
//! 两个字段各写一个 varint,与之一一对应。golden 测试钉死该布局,
//! 改动此处必须与 TA runtime 镜像(`optee_utils/tee_apps/…` 与
//! `xtee-utee`)同步。
//!
//! 帧布局: `[4 字节本机字节序 u32 长度][postcard payload]`
//! (`send_framed_message`)。
//!
//! # TODO: 统一到 teec-protocol
//!
//! Panic 通知最终也应并入 `xtee-rust-sdk` 的 `teec-protocol` 共享
//! schema;在此之前仅维持字节兼容,不再扩展本模块。

use alloc::vec::Vec;

use knet::{SendOptions, SocketOps, unix::UnixDomainSocket};
use serde::{Deserialize, Serialize};
use tee_raw_sys::TEE_ERROR_GENERIC;

use crate::tee::TeeResult;

/// legacy `TeeRequest::Panic` 的 wire 变体序号(plan 004 §4.1:Panic = 4)。
///
/// 与控制面无任何关系:控制面 `ControlRequest` 没有 Panic 变体。固定值
/// 保持对 TA runtime 镜像的字节兼容。
pub(super) const LEGACY_PANIC_VARIANT_TAG: u32 = 4;

/// legacy Panic 通知帧(旧 `TeeRequest::Panic` 的单变体兼容替身)。
///
/// 序列化结果与旧枚举变体逐字节一致:postcard 把枚举变体写成变体序号
/// varint(这里是 4)再写字段 varint;本结构体两个字段同样各写一个
/// varint。golden 测试钉死这一布局。
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LegacyPanicFrame {
    /// 恒为 [`LEGACY_PANIC_VARIANT_TAG`];字段名强调 wire 语义。
    pub variant_tag: u32,
    /// GP `TEE_Panic` 错误码。
    pub panic_code: u32,
}

impl LegacyPanicFrame {
    /// 按 `panic_code` 构造合法帧。
    pub(super) fn new(panic_code: u32) -> Self {
        LegacyPanicFrame {
            variant_tag: LEGACY_PANIC_VARIANT_TAG,
            panic_code,
        }
    }

    /// 序列化为 postcard 字节(legacy 帧的 payload)。
    pub(super) fn encode(self) -> Result<Vec<u8>, postcard::Error> {
        postcard::to_allocvec(&self)
    }
}

/// legacy 帧的 payload 上限(与旧 wire 一致,防止长度前缀滥用)。
const LEGACY_FRAME_PAYLOAD_LIMIT: usize = 64 * 1024;

fn validated_payload_len(len: usize) -> TeeResult<usize> {
    if len > LEGACY_FRAME_PAYLOAD_LIMIT {
        Err(TEE_ERROR_GENERIC)
    } else {
        Ok(len)
    }
}

/// legacy wire 发送:本机字节序长度前缀 + payload。
///
/// 单次 send 即返回:短写时由对端把不完整帧当作 EOF 处理,Panic 通知
/// 是 fire-and-forget,不重试(与旧实现一致,保持行为)。
pub(super) fn send_framed_message(socket: &UnixDomainSocket, encoded: &[u8]) -> TeeResult<()> {
    validated_payload_len(encoded.len())?;
    let mut message = Vec::with_capacity(4 + encoded.len());
    message.extend_from_slice(&(encoded.len() as u32).to_ne_bytes());
    message.extend_from_slice(encoded);
    socket
        .send(message.as_slice(), SendOptions::default())
        .map_err(|_| TEE_ERROR_GENERIC)?;
    Ok(())
}

unittest::path_mod_test!(tests, "../../ut-tests/tee/protocal/tests.rs");
