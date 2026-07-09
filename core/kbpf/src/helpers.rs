// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal Linux eBPF helper implementations.

/// Linux helper id for `bpf_trace_printk`.
pub const BPF_FUNC_TRACE_PRINTK: u32 = 6;

/// Emit a `bpf_trace_printk` message to the kernel log.
///
/// This first cut intentionally keeps formatting small: literal UTF-8
/// messages are printed directly, while format strings with arguments are
/// logged with the raw argument register values.
pub(crate) fn trace_printk(fmt: &[u8], arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let fmt_len = fmt.len() as u64;
    let fmt = trim_trailing_nul(fmt);

    match core::str::from_utf8(fmt) {
        Ok(message) if has_format_specifier(message) => {
            log::info!(
                "bpf_trace_printk: {message} [arg3={arg3:#x}, arg4={arg4:#x}, arg5={arg5:#x}]"
            );
        }
        Ok(message) => {
            log::info!("bpf_trace_printk: {message}");
        }
        Err(_) => {
            log::info!("bpf_trace_printk: <non-utf8 message, {} bytes>", fmt.len());
        }
    }

    fmt_len
}

fn trim_trailing_nul(bytes: &[u8]) -> &[u8] {
    if let Some((&0, body)) = bytes.split_last() {
        body
    } else {
        bytes
    }
}

fn has_format_specifier(message: &str) -> bool {
    let mut saw_percent = false;
    for ch in message.chars() {
        if saw_percent {
            if ch != '%' {
                return true;
            }
            saw_percent = false;
        } else if ch == '%' {
            saw_percent = true;
        }
    }
    false
}
