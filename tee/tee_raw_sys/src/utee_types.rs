// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// User-space TEE (UTEE) type definitions.
// Derived from GlobalPlatform TEE Internal Core API Specification v1.3.1
// and the OP-TEE project (https://github.com/OP-TEE/optee_os).
// These definitions conform to industry-standard specifications.
// Licensed under BSD-2-Clause as per OP-TEE project.

use core::fmt::{self, Debug};

use super::*;

#[repr(C)]
pub enum utee_time_category {
    UTEE_TIME_CAT_SYSTEM,
    UTEE_TIME_CAT_TA_PERSISTENT,
    UTEE_TIME_CAT_REE,
}

#[repr(C)]
pub enum utee_entry_func {
    UTEE_ENTRY_FUNC_OPEN_SESSION,
    UTEE_ENTRY_FUNC_CLOSE_SESSION,
    UTEE_ENTRY_FUNC_INVOKE_COMMAND,
}

#[repr(C)]
pub enum utee_cache_operation {
    TEE_CACHECLEAN,
    TEE_CACHEFLUSH,
    TEE_CACHEINVALIDATE,
}

#[repr(C)]
// SAFETY(UserRead/UserWrite): 字段只有 u64 标量与 u64 数组，任何位模式都是
// 合法且已初始化的值；全零即 types=0（四个 NONE 参数）。read_vm/write_vm 按
// 值复制不会制造未定义表示。
#[derive(Clone, Copy, Default, posix_types::UserRead, posix_types::UserWrite)]
pub struct utee_params {
    types: u64,
    vals: [u64; TEE_NUM_PARAMS as usize * 2],
}

// 布局契约（plan 004 §7.1）：`utee_params` 经 UserConstPtr::read_vm /
// write_vm 与用户态按值复制，repr(C) 布局漂移直接破坏用户 ABI，编译期钉死。
const _: () = assert!(core::mem::size_of::<utee_params>() == 72);
const _: () = assert!(core::mem::align_of::<utee_params>() == 8);
const _: () = assert!(core::mem::offset_of!(utee_params, types) == 0);
const _: () = assert!(core::mem::offset_of!(utee_params, vals) == 8);

impl utee_params {
    /// 四个参数的类型 nibble 打包值（每参数 4 位，低位是参数 0）。
    ///
    /// 参数 i 的类型 nibble = `(param_types() >> (i * 4)) & 0xF`，取值语义由
    /// GP/OP-TEE 定义（0=None … 5/6/7=Memref 类型）。本访问器只做位拆装，
    /// **不**判断合法性：非法 nibble 的拒绝发生在控制面参数转换
    /// （plan 004 §7.3），那里才知道调用方向。
    pub fn param_types(&self) -> u32 {
        self.types as u32
    }

    /// 读取第 `index` 个参数的 VALUE 槽对 (a, b)（0-based，共
    /// [`TEE_NUM_PARAMS`] 个）。
    ///
    /// MEMREF 参数复用同一对槽：请求侧 a 是用户地址、b 是长度或容量
    /// （plan 004 §7.2）；响应写回侧只改 b 为实际长度（§7.4）。`index` 由
    /// 内核侧固定循环传入，不是用户输入；越界属于编程错误，显式
    /// `assert!`（等价 slice 索引语义）而不是依赖隐式索引 —— 隐式索引在
    /// `index * 2` 溢出回绕时不会 panic，会静默读错槽。
    pub fn value(&self, index: usize) -> (u64, u64) {
        assert!(
            index < TEE_NUM_PARAMS as usize,
            "utee_params slot index out of range"
        );
        (self.vals[index * 2], self.vals[index * 2 + 1])
    }

    /// 写回第 `index` 个参数的 VALUE 槽对 (a, b)（0-based）。
    ///
    /// 只动 `vals`，不动 `types`：响应写回（§7.4）要求参数类型保持调用方
    /// 原值，只有输出长度字段允许更新。越界契约与 [`value`](Self::value)
    /// 相同：显式 `assert!`，编程错误当场暴露。
    pub fn set_value(&mut self, index: usize, a: u64, b: u64) {
        assert!(
            index < TEE_NUM_PARAMS as usize,
            "utee_params slot index out of range"
        );
        self.vals[index * 2] = a;
        self.vals[index * 2 + 1] = b;
    }
}

unittest::path_mod_test!(tests_utee_params, "../ut-tests/utee_types/tests.rs");

#[repr(C)]
#[derive(Clone, Copy, Default, posix_types::UserRead, posix_types::UserWrite)]
pub struct utee_attribute {
    pub a: u64,
    pub b: u64,
    pub attribute_id: u32,
    /// Explicitly initialized tail bytes of the 24-byte user ABI record.
    pub abi_pad: u32,
}

impl Debug for utee_attribute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.attribute_id & TEE_ATTR_FLAG_VALUE != 0 {
            write!(
                f,
                "utee_attribute::VALUE {{a: {:#010X?}, b: {:#010X?}, attribute_id: {:#010X?}}}",
                self.a, self.b, self.attribute_id
            )
        } else {
            write!(
                f,
                "utee_attribute::REF {{a: {:#010X?}, b: {:#010X?}, attribute_id: {:#010X?}}}",
                self.a, self.b, self.attribute_id
            )
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, posix_types::UserRead, posix_types::UserWrite)]
pub struct utee_object_info {
    pub obj_type: u32,
    pub obj_size: u32,
    pub max_obj_size: u32,
    pub obj_usage: u32,
    pub data_size: u32,
    pub data_pos: u32,
    pub handle_flags: u32,
}

impl Debug for utee_object_info {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "obj_type: {:#010X}, obj_size: {:#010X}, max_obj_size: {:#010X}, obj_usage: {:#010X}, \
             data_size: {:#010X}, data_pos: {:#010X}, handle_flags: {:#010X}",
            self.obj_type,
            self.obj_size,
            self.max_obj_size,
            self.obj_usage,
            self.data_size,
            self.data_pos,
            self.handle_flags
        )
    }
}
