// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(clippy::format_push_string)]

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::TeeTaCtx;

const TA_FLAG_SINGLE_INSTANCE: u32 = 1 << 2;
const TA_FLAG_MULTI_SESSION: u32 = 1 << 3;
const TA_FLAG_INSTANCE_KEEP_ALIVE: u32 = 1 << 4;
const TA_FLAG_SECURE_DATA_PATH: u32 = 1 << 5;
const TA_FLAG_REMAP_SUPPORT: u32 = 1 << 6;
const TA_FLAG_CACHE_MAINTENANCE: u32 = 1 << 7;

fn format_ta_flags(flags: u32) -> String {
    let mut names = Vec::new();
    if flags & TA_FLAG_SINGLE_INSTANCE != 0 {
        names.push("TA_FLAG_SINGLE_INSTANCE");
    }
    if flags & TA_FLAG_MULTI_SESSION != 0 {
        names.push("TA_FLAG_MULTI_SESSION");
    }
    if flags & TA_FLAG_INSTANCE_KEEP_ALIVE != 0 {
        names.push("TA_FLAG_INSTANCE_KEEP_ALIVE");
    }
    if flags & TA_FLAG_SECURE_DATA_PATH != 0 {
        names.push("TA_FLAG_SECURE_DATA_PATH");
    }
    if flags & TA_FLAG_REMAP_SUPPORT != 0 {
        names.push("TA_FLAG_REMAP_SUPPORT");
    }
    if flags & TA_FLAG_CACHE_MAINTENANCE != 0 {
        names.push("TA_FLAG_CACHE_MAINTENANCE");
    }
    if names.is_empty() {
        String::from("()")
    } else {
        format!("({})", names.join("|"))
    }
}

pub fn has_ta_info(ta_ctx: &TeeTaCtx) -> bool {
    ta_ctx.uuid != uuid::Uuid::default().to_string()
}

pub fn render_ta_ctx_uuid(ta_ctx: &TeeTaCtx) -> Vec<u8> {
    format!("{}\n", ta_ctx.uuid).into_bytes()
}

pub fn render_ta_head(ta_ctx: &TeeTaCtx) -> Vec<u8> {
    let h = &ta_ctx.ta_head;
    let uuid = uuid::Uuid::from_fields(
        h.uuid.timeLow,
        h.uuid.timeMid,
        h.uuid.timeHiAndVersion,
        &h.uuid.clockSeqAndNode,
    )
    .map(|u| u.to_string().to_uppercase())
    .unwrap_or_else(|_| String::from("00000000-0000-0000-0000-000000000000"));
    let flags_text = format_ta_flags(h.flags);
    format!(
        "uuid: {}\nstack_size: {:#010x}\nflags: {:#010x} {}\ndepr_entry: {:#018x}\n",
        uuid, h.stack_size, h.flags, flags_text, h.depr_entry
    )
    .into_bytes()
}
