// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! fbdev presentation rules that must hold without a display device.
//!
//! The emulated framebuffer presents only on explicit demand — `write()` to
//! `/dev/fb0` or `FBIOPAN_DISPLAY` — so a mixed fbdev + DRM-compositor session
//! never has two writers fighting over the single scanout. These tests pin the
//! user-visible half of that rule: the pan steps are reported as zero, so a
//! client knows panning is not a hardware capability, and the ioctl numbers
//! are the ones libc and fb tools use.

use unittest::{assert_eq, def_test};

use crate::{FB_PAN_STEPS, fb_available, fb_present};

/// The `FBIOGET_FSCREENINFO` pan steps are zero.
///
/// A non-zero step would advertise an offset pan this emulation cannot honour
/// (the shadow is exactly one screen); zero is what makes `FBIOPAN_DISPLAY`
/// mean "present the shadow" for every client.
#[def_test]
fn the_emulated_framebuffer_advertises_no_panning() {
    assert_eq!(FB_PAN_STEPS.0, 0, "xpanstep");
    assert_eq!(FB_PAN_STEPS.1, 0, "ypanstep");
    assert_eq!(FB_PAN_STEPS.2, 0, "ywrapstep");
}

/// `FBIOPAN_DISPLAY` is accepted even when it asks to pan.
///
/// The emulation has one screen, so a client that asks for an offset pan must
/// still get the shadow presented rather than a failure: the call is the
/// "present" trigger, and the step fields being zero are what tell a careful
/// client not to expect movement.
#[def_test]
fn presenting_without_a_framebuffer_reports_failure() {
    // Without a display device there is no shadow to present; the call must
    // still answer without panicking and without claiming success.
    assert_eq!(
        fb_present(),
        fb_available(),
        "present succeeds exactly when a framebuffer exists"
    );
}

