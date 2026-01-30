// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

// Stub symbols for test mode
// These symbols are normally provided by the kernel linker script,
// but we need stub implementations for user-space tests.

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_abbrev: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_abbrev: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_addr: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_addr: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_aranges: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_aranges: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_info: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_info: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_line: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_line: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_line_str: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_line_str: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_ranges: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_ranges: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_rnglists: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_rnglists: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_str: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_str: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __start_debug_str_offsets: [u8; 0] = [];

#[unsafe(no_mangle)]
#[used]
pub static __stop_debug_str_offsets: [u8; 0] = [];
