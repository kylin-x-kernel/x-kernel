// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Placeholder for the `.init_array` section, so that
/// the `__init_array_start` and `__init_array_end` symbols can be generated.
#[unsafe(link_section = ".init_array")]
#[used]
static _SECTION_PLACE_HOLDER: [u8; 0] = [];

// SAFETY: The linker script exports `__init_array_start` and
// `__init_array_end` as the bounds of the contiguous `.init_array` region.
unsafe extern "C" {
    fn __init_array_start();
    fn __init_array_end();
}

/// Invoke all init functions registered by the `register_init` attribute.
///
/// # Notes
/// Caller should ensure that the `.init_array` section will not be disturbed by other sections.
pub(crate) fn init_cb() {
    let init_start = __init_array_start as *const () as *const extern "C" fn();
    let init_end = __init_array_end as *const () as *const extern "C" fn();

    // SAFETY: The linker provides a contiguous `.init_array` range containing
    // only `extern "C" fn()` entries emitted by `#[register_init]`, so
    // `[init_start, init_end)` can be viewed as a function-pointer slice.
    let init_fns = unsafe {
        core::slice::from_raw_parts(init_start, init_end.offset_from(init_start) as usize)
    };

    for init_fn in init_fns {
        init_fn();
    }
}
