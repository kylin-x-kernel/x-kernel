// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Internal profiling state.

/// Tracks whether profile data has already been dumped.
static mut PROFILE_DUMPED: u32 = 0;

/// Returns non-zero if the profile data has already been dumped.
pub fn is_profile_dumped() -> u32 {
    // SAFETY: Matches the C runtime semantics (single-threaded access).
    unsafe { PROFILE_DUMPED }
}

/// Sets the profile dumped flag.
pub fn set_profile_dumped(value: u32) {
    // SAFETY: Matches the C runtime semantics (single-threaded access).
    unsafe {
        PROFILE_DUMPED = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumped_flag_roundtrip() {
        set_profile_dumped(0);
        assert_eq!(is_profile_dumped(), 0);
        set_profile_dumped(1);
        assert_eq!(is_profile_dumped(), 1);
        set_profile_dumped(0);
    }
}
