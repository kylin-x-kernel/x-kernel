// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! inter-TA 三个 syscall 的 manager 控制面实现(plan 004 §6.4–§6.6)。
//!
//! 调用方 TA 的 `TEE_OpenTASession` / `TEE_InvokeTACommand` /
//! `TEE_CloseTASession` 不再直连目标 TA socket,而是经
//! [`tee_ta_manager`](crate::tee::tee_ta_manager) 的控制面客户端与
//! vsock-manager 交互。本地 handle 隐藏 manager session id(§6.2);Value/
//! Memref 参数按 §7 转换;错误与 origin 按 §6.7 由 wire 响应携带,本地
//! 失败恒 `TEE_ORIGIN_API`,传输失败恒 `TEE_ORIGIN_COMMS`,不从错误码
//! 猜测来源。

use alloc::{string::ToString, vec::Vec};
use core::ffi::{c_uint, c_ulong};

use osvm::MemError;
use posix_types::{UserConstPtr, UserPtr};
use tee_raw_sys::{
    TEE_ERROR_ACCESS_DENIED, TEE_ERROR_BAD_FORMAT, TEE_ERROR_BAD_PARAMETERS,
    TEE_ERROR_COMMUNICATION, TEE_ERROR_GENERIC, TEE_ERROR_ITEM_NOT_FOUND, TEE_ERROR_OUT_OF_MEMORY,
    TEE_ERROR_SHORT_BUFFER, TEE_ERROR_TARGET_DEAD, TEE_ORIGIN_API, TEE_ORIGIN_COMMS,
    TEE_ORIGIN_TEE, TEE_SUCCESS, TEE_UUID, utee_params,
};

use crate::tee::{
    TeeResult,
    control_protocol::{
        ControlRequest, ControlResponse, TEE_Param, TEE_ParamType, TEE_Parameter, TEE_Parameters,
        TEE_Value,
    },
    tee_session::{with_tee_ta_ctx, with_tee_ta_ctx_mut},
    tee_ta_manager::{CONTROL_SOCKET_PATH, control_exchange_at, control_op_timeout},
    uuid::Uuid,
};

/// §7.3 参数预算:请求侧输入 data 合计、输出/inout 容量合计、响应侧
/// 输出 data 合计各自不超过 60 KiB。这是给四个参数与 envelope 留出的
/// 数据预算,不是绕过 65,536 帧上限的另一种写法。
const INTER_TA_PARAM_BUDGET: usize = 60 * 1024;

/// 参数下标数量(GP `TEE_NUM_PARAMS`,固定 4)。
const PARAM_COUNT: usize = 4;

/// §6.7:syscall 层的 result/origin 对。
///
/// 本地失败(参数、用户地址、本地 handle)恒 `TEE_ORIGIN_API`;传输与
/// 协议失败恒 `TEE_ORIGIN_COMMS`;manager/TA 失败透传 wire origin。
#[derive(Clone, Copy, Debug)]
struct TeeFailure {
    result: u32,
    origin: u32,
}

impl TeeFailure {
    /// 本地失败(x-kernel 参数、用户地址、本地 handle,§6.7 表第 1 行)。
    fn local(result: u32) -> Self {
        TeeFailure {
            result,
            origin: TEE_ORIGIN_API,
        }
    }

    /// 传输/协议失败(§6.7 表第 2 行)。
    fn comms(result: u32) -> Self {
        TeeFailure {
            result,
            origin: TEE_ORIGIN_COMMS,
        }
    }

    /// wire 响应携带的失败(manager 路由或目标 TA,§6.7 表第 3-5 行)。
    fn from_wire(result: u32, origin: u32) -> Self {
        TeeFailure { result, origin }
    }
}

type InterTaResult<T> = Result<T, TeeFailure>;

/// 用户内存访问错误 → 本地 `BAD_PARAMETERS`/`GENERIC`。
fn map_user_mem_error(err: MemError) -> u32 {
    match err {
        MemError::OutOfMemory => TEE_ERROR_OUT_OF_MEMORY,
        MemError::InvalidAddr | MemError::NoAccess => TEE_ERROR_BAD_PARAMETERS,
        _ => TEE_ERROR_GENERIC,
    }
}

/// 用户内存访问错误 → 本地失败(origin 恒 API)。
fn map_user_mem_failure(err: MemError) -> TeeFailure {
    TeeFailure::local(map_user_mem_error(err))
}

/// 受检写回 `returnOrigin`(§6.4 步骤 13 / §6.5 步骤 11)。
fn write_return_origin(ret_orig: *mut c_uint, origin: u32) -> TeeResult {
    UserPtr::<u32>::from(ret_orig)
        .write_vm(origin)
        .map_err(map_user_mem_error)?;
    Ok(())
}

/// 把内部失败转换成 syscall 返回值:先受检写回 origin(写失败不掩盖
/// 原始错误,仅记录),再返回 `Err(result)`。
fn finish_with_origin(failure: TeeFailure, ret_orig: *mut c_uint) -> TeeResult {
    if let Err(err) = write_return_origin(ret_orig, failure.origin) {
        debug!(
            "inter-ta returnOrigin write failed: {:?} (result={:#010x})",
            err, failure.result
        );
    }
    Err(failure.result)
}

// ---------------------------------------------------------------------------
// §7 参数转换
// ---------------------------------------------------------------------------

/// §7.3:非法 type nibble(4、8..15)直接拒绝,不能转换成 NONE。
fn wire_param_type(nibble: u32) -> InterTaResult<TEE_ParamType> {
    let param_type = match nibble {
        0 => TEE_ParamType::None,
        1 => TEE_ParamType::ValueInput,
        2 => TEE_ParamType::ValueOutput,
        3 => TEE_ParamType::ValueInout,
        5 => TEE_ParamType::MemrefInput,
        6 => TEE_ParamType::MemrefOutput,
        7 => TEE_ParamType::MemrefInout,
        _ => return Err(TeeFailure::local(TEE_ERROR_BAD_PARAMETERS)),
    };
    Ok(param_type)
}

/// §7.3 memref 地址/长度组合:`b == 0` 允许 `a == NULL`;
/// `b != 0` 时 `a == NULL` → BAD_PARAMETERS。
fn check_memref_addr_len(addr: u64, len: u64) -> InterTaResult<()> {
    if len != 0 && addr == 0 {
        return Err(TeeFailure::local(TEE_ERROR_BAD_PARAMETERS));
    }
    Ok(())
}

/// §7 取第 `index` 个参数(TEE_Parameters 是 4 字段元组结构)。
fn param_at(params: &TEE_Parameters, index: usize) -> &TEE_Parameter {
    match index {
        0 => &params.0,
        1 => &params.1,
        2 => &params.2,
        _ => &params.3,
    }
}

/// u64 槽值 → usize(checked;拒绝截断)。
fn slot_len(value: u64) -> InterTaResult<usize> {
    usize::try_from(value).map_err(|_| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))
}

/// §7.2:VALUE 取 a/b 的低 32 位。
fn low32(value: u64) -> u32 {
    (value & 0xFFFF_FFFF) as u32
}

/// 受检读取用户 memref 数据。
///
/// §7.3:`b == 0` 允许 `a == NULL`:零长度读取不触碰用户内存,
/// 直接返回空 Vec(osvm 对空区间的 NULL 地址会报 InvalidAddr)。
fn read_user_memref(addr: u64, len: u64) -> InterTaResult<Vec<u8>> {
    let len = slot_len(len)?;
    if len == 0 {
        return Ok(Vec::new());
    }
    UserConstPtr::<u8>::from(addr as *const u8)
        .load_vm_vec(len)
        .map_err(map_user_mem_failure)
}

/// §7.2/§7.3:用户 `utee_params` → wire `TEE_Parameters`。
///
/// 两遍扫描:第一遍校验全部 4 个 type nibble 并在触碰用户数据之前校验
/// 合计预算(输入/inout data 合计、输出/inout 容量合计,checked
/// arithmetic);第二遍按 §7.2 表逐参数构造并复制输入数据。
fn wire_params_from_user(up: &utee_params) -> InterTaResult<TEE_Parameters> {
    let types = up.param_types();

    let mut input_data_total: usize = 0;
    let mut output_capacity_total: usize = 0;
    for index in 0..PARAM_COUNT {
        let nibble = (types >> (index * 4)) & 0xF;
        let param_type = wire_param_type(nibble)?;
        let (_, b) = up.value(index);
        match param_type {
            TEE_ParamType::MemrefInput => {
                input_data_total = input_data_total
                    .checked_add(slot_len(b)?)
                    .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;
            }
            TEE_ParamType::MemrefOutput => {
                output_capacity_total = output_capacity_total
                    .checked_add(slot_len(b)?)
                    .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;
            }
            // §7.3:inout 的 b 同时是输入长度与容量,计入两个合计。
            TEE_ParamType::MemrefInout => {
                let len = slot_len(b)?;
                input_data_total = input_data_total
                    .checked_add(len)
                    .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;
                output_capacity_total = output_capacity_total
                    .checked_add(len)
                    .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;
            }
            _ => {}
        }
    }
    if input_data_total > INTER_TA_PARAM_BUDGET || output_capacity_total > INTER_TA_PARAM_BUDGET {
        return Err(TeeFailure::local(TEE_ERROR_BAD_PARAMETERS));
    }

    let mut params = [
        TEE_Parameter::default(),
        TEE_Parameter::default(),
        TEE_Parameter::default(),
        TEE_Parameter::default(),
    ];
    for (index, slot) in params.iter_mut().enumerate() {
        let nibble = (types >> (index * 4)) & 0xF;
        let param_type = wire_param_type(nibble)?;
        let (a, b) = up.value(index);
        let param = match param_type {
            TEE_ParamType::None => TEE_Param::default(),
            // §7.2:VALUE_INPUT/INOUT 取 a/b 低 32 位。
            TEE_ParamType::ValueInput | TEE_ParamType::ValueInout => TEE_Param {
                data: Vec::new(),
                values: TEE_Value {
                    a: low32(a),
                    b: low32(b),
                },
            },
            TEE_ParamType::ValueOutput => TEE_Param::default(),
            TEE_ParamType::MemrefInput | TEE_ParamType::MemrefInout => {
                check_memref_addr_len(a, b)?;
                TEE_Param {
                    data: read_user_memref(a, b)?,
                    values: TEE_Value::default(),
                }
            }
            // §7.2:MEMREF_OUTPUT 以 values.a 携带容量(teec-protocol 约定)。
            TEE_ParamType::MemrefOutput => {
                check_memref_addr_len(a, b)?;
                TEE_Param {
                    data: Vec::new(),
                    values: TEE_Value { a: low32(b), b: 0 },
                }
            }
        };
        *slot = TEE_Parameter { param, param_type };
    }
    Ok(TEE_Parameters(
        params[0].clone(),
        params[1].clone(),
        params[2].clone(),
        params[3].clone(),
    ))
}

/// §7.4 响应参数写回结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParamWriteback {
    /// TA result 成功:输出 data/值对已写回用户内存。
    OutputsApplied,
    /// SHORT_BUFFER(TA 主动或内核检测):仅 required length 写回,
    /// 未复制任何 output/inout data。
    ShortBuffer,
    /// 其他错误:不写回输出,调用方长度字段保持原值。
    NoOutputs,
}

/// §7.4:把 wire 响应参数写回用户 `utee_params`(内核副本)。
///
/// 先验证四个 response 参数的类型与长度,再决定是否触碰用户内存:
/// 类型与请求 nibble 不一致 → `BAD_FORMAT + COMMS`;任何 output/inout
/// 溢出时不复制任何 output/inout data,避免部分写回。输出 data 先落
/// 用户内存,长度字段最后写入 `up` 并由调用方统一写回 —— 坏地址时
/// 用户侧看不到已提交的逻辑结果(长度未变,data 无语义)。
fn apply_response_params(
    up: &mut utee_params,
    response: &TEE_Parameters,
    ta_result: u32,
) -> InterTaResult<ParamWriteback> {
    let types = up.param_types();

    // 第一遍:类型一致性、响应 data 合计、成功路径容量。
    let mut response_data_total: usize = 0;
    let mut any_overflow = false;
    for index in 0..PARAM_COUNT {
        let request_type = wire_param_type((types >> (index * 4)) & 0xF)?;
        let resp_param = param_at(response, index);
        if resp_param.param_type != request_type {
            // §7.4:类型不一致按 malformed response 处理。
            return Err(TeeFailure::comms(TEE_ERROR_BAD_FORMAT));
        }
        if matches!(
            request_type,
            TEE_ParamType::MemrefOutput | TEE_ParamType::MemrefInout
        ) {
            let data_len = resp_param.param.data.len();
            response_data_total = response_data_total
                .checked_add(data_len)
                .ok_or_else(|| TeeFailure::comms(TEE_ERROR_BAD_FORMAT))?;
            if ta_result == TEE_SUCCESS {
                let (_, capacity) = up.value(index);
                if data_len > slot_len(capacity)? {
                    any_overflow = true;
                }
            }
        }
    }
    // §7.3:响应 data 合计预算。
    if response_data_total > INTER_TA_PARAM_BUDGET {
        return Err(TeeFailure::comms(TEE_ERROR_BAD_FORMAT));
    }

    if ta_result != TEE_SUCCESS && ta_result != TEE_ERROR_SHORT_BUFFER {
        // §7.4:其他错误不复制 output/inout data,保留调用方原有输出长度。
        return Ok(ParamWriteback::NoOutputs);
    }

    if ta_result == TEE_ERROR_SHORT_BUFFER {
        // TA 主动 SHORT_BUFFER:required length 只取对应 response 参数的
        // values.a;即使 data 恰好不超过原容量,也不复制任何数据。
        for index in 0..PARAM_COUNT {
            let request_type = wire_param_type((types >> (index * 4)) & 0xF)?;
            if !matches!(
                request_type,
                TEE_ParamType::MemrefOutput | TEE_ParamType::MemrefInout
            ) {
                continue;
            }
            let resp_param = param_at(response, index);
            let required = resp_param.param.values.a as usize;
            // §7.4:values.a 必须不小于同参数 data 长度且不超过 60 KiB。
            if required < resp_param.param.data.len() || required > INTER_TA_PARAM_BUDGET {
                return Err(TeeFailure::comms(TEE_ERROR_BAD_FORMAT));
            }
            let (addr, _) = up.value(index);
            up.set_value(index, addr, required as u64);
        }
        return Ok(ParamWriteback::ShortBuffer);
    }

    if any_overflow {
        // 内核检测 data 超容量:不复制任何 output/inout data,各自长度改为
        // 该参数 response data 的实际长度,返回内核生成的 SHORT_BUFFER。
        for index in 0..PARAM_COUNT {
            let request_type = wire_param_type((types >> (index * 4)) & 0xF)?;
            if !matches!(
                request_type,
                TEE_ParamType::MemrefOutput | TEE_ParamType::MemrefInout
            ) {
                continue;
            }
            let (addr, _) = up.value(index);
            let data_len = param_at(response, index).param.data.len() as u64;
            up.set_value(index, addr, data_len);
        }
        return Ok(ParamWriteback::ShortBuffer);
    }

    // 成功:输出 data 先写用户内存,再更新值对与长度(§6.5 步骤 7/10 次序)。
    for index in 0..PARAM_COUNT {
        let request_type = wire_param_type((types >> (index * 4)) & 0xF)?;
        let resp_param = param_at(response, index);
        match request_type {
            // §7.4:值对仅在 TA result 成功时写回。
            TEE_ParamType::ValueOutput | TEE_ParamType::ValueInout => {
                up.set_value(
                    index,
                    resp_param.param.values.a as u64,
                    resp_param.param.values.b as u64,
                );
            }
            TEE_ParamType::MemrefOutput | TEE_ParamType::MemrefInout => {
                let (addr, _) = up.value(index);
                write_user_memref(addr, &resp_param.param.data)?;
                up.set_value(index, addr, resp_param.param.data.len() as u64);
            }
            // NONE/VALUE_INPUT/MEMREF_INPUT:不写回(§7.4)。
            _ => {}
        }
    }
    Ok(ParamWriteback::OutputsApplied)
}

/// 受检写回用户 memref 数据;空 data 直接成功(§7.3 `b == 0` 语义)。
fn write_user_memref(addr: u64, data: &[u8]) -> InterTaResult<()> {
    if data.is_empty() {
        return Ok(());
    }
    UserPtr::<u8>::from(addr as *mut u8)
        .write_vm_slice(data)
        .map_err(map_user_mem_failure)
}

// ---------------------------------------------------------------------------
// §6.4/§6.5/§6.6 syscalls
// ---------------------------------------------------------------------------

/// 控制面交换的统一错误映射(§6.3:一律 COMMUNICATION + COMMS)。
fn exchange(request: ControlRequest) -> Result<ControlResponse, TeeFailure> {
    control_exchange_at(CONTROL_SOCKET_PATH, request, control_op_timeout())
        .map_err(|_| TeeFailure::comms(TEE_ERROR_COMMUNICATION))
}

/// §6.4:`TEE_OpenTASession`。
///
/// Open 参数只有输入语义:响应不携带 params,不伪造输出参数响应
/// (§7.4 末段);本地 handle 分配或 `ta_sees` 写回失败按契约回滚
/// manager session。
pub(super) fn sys_tee_scn_open_ta_session(
    dest: *const TEE_UUID,
    _cancel_req_to: c_ulong,
    usr_param: *mut utee_params,
    ta_sees: *mut c_uint,
    ret_orig: *mut c_uint,
) -> TeeResult {
    match open_ta_session_inner(dest, usr_param, ta_sees) {
        Ok(()) => {
            // §6.4 步骤 13:成功时 origin 写 TEE;调用方只应在失败时解释。
            // ta_sees 已写回、session 已登记:handle 已交付,按内存错误
            // 返回,不回滚(设计未对 returnOrigin 失败规定回滚)。
            write_return_origin(ret_orig, TEE_ORIGIN_TEE)?;
            Ok(())
        }
        Err(failure) => finish_with_origin(failure, ret_orig),
    }
}

fn open_ta_session_inner(
    dest: *const TEE_UUID,
    usr_param: *mut utee_params,
    ta_sees: *mut c_uint,
) -> InterTaResult<()> {
    // 步骤 1-3:UUID、参数、写回地址受检读取。
    let uuid = UserConstPtr::<TEE_UUID>::from(dest)
        .read_vm()
        .map_err(map_user_mem_failure)?;
    let uuid_string = Uuid::from(uuid).to_string();
    let up = UserConstPtr::<utee_params>::from(usr_param.cast_const())
        .read_vm()
        .map_err(map_user_mem_failure)?;
    let session_out = UserPtr::<u32>::from(ta_sees)
        .check_non_null()
        .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;

    // 步骤 4-5:类型/地址校验 + wire 转换。
    let params = wire_params_from_user(&up)?;

    // 步骤 6-7:控制面往返。
    let response = exchange(ControlRequest::OpenSession {
        uuid: uuid_string.clone(),
        connection_method: 0,
        params,
    })?;
    let ControlResponse::OpenSession {
        session_id,
        result,
        origin,
    } = response
    else {
        // 客户端已严格校验变体;此分支防御式兜底。
        return Err(TeeFailure::comms(TEE_ERROR_COMMUNICATION));
    };

    // 步骤 8:失败透传 wire origin(§6.7)。
    if result != TEE_SUCCESS {
        return Err(TeeFailure::from_wire(result, origin));
    }
    // 步骤 9:成功但 session id 为 0 → 通信错误。
    if session_id == 0 {
        return Err(TeeFailure::comms(TEE_ERROR_COMMUNICATION));
    }

    // 步骤 10-11:本地 handle 分配(表锁只覆盖表操作,§6.2;失败回滚
    // 在锁外执行,不跨网络 I/O 持锁)。
    let inserted = with_tee_ta_ctx_mut(|ctx| {
        // 满表 → OUT_OF_MEMORY(§6.2)。
        Ok(ctx
            .insert_session(uuid_string, session_id)
            .ok_or(TEE_ERROR_OUT_OF_MEMORY))
    })
    .map_err(|_| TeeFailure::local(TEE_ERROR_GENERIC))?;
    let handle = match inserted {
        Ok(handle) => handle,
        Err(result) => {
            // §6.4:分配失败必须立即关闭已打开的 manager session;
            // 关闭失败只记录日志。
            rollback_manager_session(session_id);
            return Err(TeeFailure::local(result));
        }
    };

    // 步骤 12:写回 handle;失败不能留下本地 handle。
    if session_out.write_vm(handle).is_err() {
        // 先摘除本地表项,再尝试关闭 manager session。
        let _ = with_tee_ta_ctx_mut(|ctx| Ok(ctx.remove_session(handle)));
        rollback_manager_session(session_id);
        return Err(TeeFailure::local(TEE_ERROR_BAD_PARAMETERS));
    }
    Ok(())
}

/// §6.4 回滚:关闭刚打开的 manager session;失败仅记日志。
fn rollback_manager_session(session_id: u32) {
    if let Err(failure) = exchange(ControlRequest::CloseSession { session_id }) {
        warn!(
            "open rollback close failed: result={:#010x} (manager session {session_id})",
            failure.result
        );
    }
}

/// §6.5:`TEE_InvokeTACommand`。
pub(super) fn sys_tee_scn_invoke_ta_command(
    ta_sees: c_ulong,
    cancel_req_to: c_ulong,
    cmd_id: c_ulong,
    usr_param: *mut utee_params,
    ret_orig: *mut c_uint,
) -> TeeResult {
    let result = (|| {
        let timeout_ms = u32::try_from(cancel_req_to)
            .map_err(|_| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?;
        // Compute before user copies or socket work; queueing cannot restart this clock.
        let deadline_ns = if timeout_ms == u32::MAX {
            None
        } else {
            Some(
                ktask::monotonic_time()
                    .checked_add(ktime_types::TimeSpan::from_millis(timeout_ms as u64))
                    .ok_or_else(|| TeeFailure::local(TEE_ERROR_BAD_PARAMETERS))?
                    .as_nanos_u64_saturating(),
            )
        };
        invoke_ta_command_inner(ta_sees as u32, cmd_id as u32, usr_param, deadline_ns)
    })();
    match result {
        Ok(()) => {
            // §6.7:成功时 origin 写 TEE。
            write_return_origin(ret_orig, TEE_ORIGIN_TEE)?;
            Ok(())
        }
        Err(failure) => finish_with_origin(failure, ret_orig),
    }
}

fn invoke_ta_command_inner(
    handle: u32,
    cmd_id: u32,
    usr_param: *mut utee_params,
    deadline_ns: Option<u64>,
) -> InterTaResult<()> {
    // 步骤 1-2:本地 handle 查表;找不到在发网络请求前失败。
    let identity = with_tee_ta_ctx(|ctx| Ok(ctx.session(handle).cloned()))
        .map_err(|_| TeeFailure::local(TEE_ERROR_GENERIC))?
        .ok_or_else(|| TeeFailure::local(TEE_ERROR_ITEM_NOT_FOUND))?;

    // 步骤 3-4:读取并转换参数。
    let mut up = UserConstPtr::<utee_params>::from(usr_param.cast_const())
        .read_vm()
        .map_err(map_user_mem_failure)?;
    let params = wire_params_from_user(&up)?;

    // 步骤 5-6:控制面往返(manager global session id)。
    let request = match deadline_ns {
        Some(deadline_ns) => ControlRequest::InvokeCommandWithTimeout {
            session_id: identity.manager_session_id,
            cmd_id,
            params,
            deadline_ns,
        },
        None => ControlRequest::InvokeCommand {
            session_id: identity.manager_session_id,
            cmd_id,
            params,
        },
    };
    let response = exchange(request)?;
    let ControlResponse::InvokeCommand {
        params: resp_params,
        result,
        origin,
    } = response
    else {
        return Err(TeeFailure::comms(TEE_ERROR_COMMUNICATION));
    };

    // 步骤 7-9:响应参数应用(§7.4)。
    let writeback = apply_response_params(&mut up, &resp_params, result)?;

    // A TA's missing object is a command error, not a missing manager session.
    // Only the manager's TEE-origin ITEM_NOT_FOUND invalidates this handle;
    // owner mismatch has its own ACCESS_DENIED/API signature (§5.5).
    let missing_session = result == TEE_ERROR_ITEM_NOT_FOUND && origin == TEE_ORIGIN_TEE;
    let owner_mismatch = result == TEE_ERROR_ACCESS_DENIED && origin == TEE_ORIGIN_API;
    if missing_session || result == TEE_ERROR_TARGET_DEAD || owner_mismatch {
        remove_session_entry(handle);
    }

    // 步骤 10:仅在被更新时写回 utee_params(成功或 SHORT_BUFFER);
    // 其他错误保留调用方原有结构。
    if writeback != ParamWriteback::NoOutputs {
        UserPtr::<utee_params>::from(usr_param)
            .write_vm(up)
            .map_err(map_user_mem_failure)?;
    }

    // 步骤 11-12:origin 与 result。
    match writeback {
        ParamWriteback::OutputsApplied if result == TEE_SUCCESS => Ok(()),
        // 成功写回完成但 result 非成功:透传 wire origin。
        ParamWriteback::OutputsApplied => Err(TeeFailure::from_wire(result, origin)),
        // 内核检测容量不足生成的 SHORT_BUFFER:origin 属 x-kernel 参数类。
        ParamWriteback::ShortBuffer if result == TEE_SUCCESS => {
            Err(TeeFailure::local(TEE_ERROR_SHORT_BUFFER))
        }
        // TA 主动 SHORT_BUFFER:透传 TA origin。
        ParamWriteback::ShortBuffer => Err(TeeFailure::from_wire(result, origin)),
        ParamWriteback::NoOutputs => Err(TeeFailure::from_wire(result, origin)),
    }
}

/// §6.6:`TEE_CloseTASession`。Close 没有 `returnOrigin` 参数。
///
/// manager 返回成功、`ITEM_NOT_FOUND`、`TARGET_DEAD` 或通信错误时都
/// 删除本地表项(§5.6.10:该 handle 已不能安全复用);错误码返回上层,
/// 现有 rust-libutee 按 API 约定触发 `TEE_Panic`。
pub(super) fn sys_tee_scn_close_ta_session(ta_sees: c_ulong) -> TeeResult {
    // 步骤 1-2:本地 handle 查表。
    let handle = ta_sees as u32;
    let Some(identity) =
        with_tee_ta_ctx(|ctx| Ok(ctx.session(handle).cloned())).map_err(|_| TEE_ERROR_GENERIC)?
    else {
        return Err(TEE_ERROR_ITEM_NOT_FOUND);
    };

    // 步骤 3:发送 CloseSession。
    match exchange(ControlRequest::CloseSession {
        session_id: identity.manager_session_id,
    }) {
        Ok(ControlResponse::CloseSession { result, .. }) => {
            // A decoded communication failure also means the manager claimed
            // cleanup; this handle must not submit another Invoke or Close.
            // Other errors (such as owner mismatch) retain the entry (§6.6).
            if matches!(
                result,
                TEE_SUCCESS
                    | TEE_ERROR_ITEM_NOT_FOUND
                    | TEE_ERROR_TARGET_DEAD
                    | TEE_ERROR_COMMUNICATION
            ) {
                remove_session_entry(handle);
            }
            if result == TEE_SUCCESS {
                Ok(())
            } else {
                Err(result)
            }
        }
        // A failed exchange cannot confirm the manager claimed Close. Invalidate
        // the handle, but retain the owner-level exit cleanup obligation.
        Err(failure) => {
            remove_session_with_uncertain_close(handle);
            Err(failure.result)
        }
        // 客户端已严格校验变体;此分支防御式兜底。
        Ok(_) => {
            remove_session_with_uncertain_close(handle);
            Err(TEE_ERROR_COMMUNICATION)
        }
    }
}

fn remove_session_with_uncertain_close(handle: u32) {
    let _ = with_tee_ta_ctx_mut(|ctx| {
        ctx.remove_session(handle);
        ctx.require_owner_release();
        Ok(())
    });
}

fn remove_session_entry(handle: u32) {
    let _ = with_tee_ta_ctx_mut(|ctx| {
        ctx.remove_session(handle);
        Ok(())
    });
}

unittest::path_mod_test!(tests, "../../ut-tests/tee/tee_inter_ta/tests.rs");
