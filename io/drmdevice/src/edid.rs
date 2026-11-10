// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! EDID detailed timing descriptors, the mode source of the connector.
//!
//! The base block carries up to four 18-byte Detailed Timing Descriptors
//! behind the established and standard timing sections; the first is the
//! panel's preferred timing. This module only reads what a mode needs and
//! leaves the descriptor text blocks to whoever wants the monitor name, so
//! the parser is pure bytes-in, timings-out and unit-testable without a card.

use alloc::vec::Vec;

use crate::{decimal::write_decimal, prelude::*};

/// One timing an EDID names, with the blanking a mode needs to be self-consistent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EdidTiming {
    /// Pixel clock in kHz.
    pub(crate) clock: u32,
    /// Visible and blanked columns.
    pub(crate) hdisplay: u16,
    pub(crate) hsync_start: u16,
    pub(crate) hsync_end: u16,
    pub(crate) htotal: u16,
    /// Visible and blanked rows.
    pub(crate) vdisplay: u16,
    pub(crate) vsync_start: u16,
    pub(crate) vsync_end: u16,
    pub(crate) vtotal: u16,
    /// Whether the timing names an interlaced frame.
    pub(crate) interlaced: bool,
}

/// The first detailed timing descriptor of the base block, at byte 54.
const FIRST_DTD: usize = 54;
/// Detailed timing descriptor length, and the four the base block holds.
const DTD_SIZE: usize = 18;
const DTD_COUNT: usize = 4;

/// Reads every detailed timing descriptor of one EDID base block.
///
/// A slot that does not hold a timing (pixel clock zero, the marker of a
/// monitor descriptor) ends the list: the specification fills unused slots
/// from the end.
pub(crate) fn detailed_timings(edid: &[u8]) -> Vec<EdidTiming> {
    let mut timings = Vec::new();
    for slot in 0..DTD_COUNT {
        let start = FIRST_DTD + slot * DTD_SIZE;
        let Some(descriptor) = edid.get(start..start + DTD_SIZE) else {
            break;
        };
        let Some(timing) = timing(descriptor) else {
            break;
        };
        timings.push(timing);
    }
    timings
}

/// The wire modes one EDID describes, the first timing the preferred one.
///
/// An EDID without a detailed timing names no mode, and the caller falls back
/// to the display's own geometry.
pub(crate) fn modes(edid: &[u8]) -> Vec<DrmModeModeInfo> {
    detailed_timings(edid)
        .into_iter()
        .enumerate()
        .map(|(index, timing)| mode(timing, index == 0))
        .collect()
}

/// Decodes one 18-byte descriptor, or `None` when it is not a timing.
fn timing(descriptor: &[u8]) -> Option<EdidTiming> {
    // A pixel clock of zero marks a monitor descriptor (name, limits, color
    // point), not a timing.
    let clock = u16::from(descriptor[0]) | (u16::from(descriptor[1]) << 8);
    if clock == 0 {
        return None;
    }
    let hactive = u16::from(descriptor[2]) | (u16::from(descriptor[4] & 0xf0) << 4);
    let hblank = u16::from(descriptor[3]) | (u16::from(descriptor[4] & 0x0f) << 8);
    let vactive = u16::from(descriptor[5]) | (u16::from(descriptor[7] & 0xf0) << 4);
    let vblank = u16::from(descriptor[6]) | (u16::from(descriptor[7] & 0x0f) << 8);
    let hsync_offset = u16::from(descriptor[8]) | (u16::from(descriptor[11] >> 6) << 8);
    let hsync_width = u16::from(descriptor[9]) | (u16::from(descriptor[11] >> 4 & 0x3) << 8);
    let vsync_offset = u16::from(descriptor[10] >> 4) | (u16::from(descriptor[11] >> 2 & 0x3) << 4);
    let vsync_width = u16::from(descriptor[10] & 0x0f) | (u16::from(descriptor[11] & 0x3) << 4);
    let hsync_start = hactive.checked_add(hsync_offset)?;
    let vsync_start = vactive.checked_add(vsync_offset)?;
    Some(EdidTiming {
        clock: u32::from(clock) * 10,
        hdisplay: hactive,
        hsync_start,
        hsync_end: hsync_start.checked_add(hsync_width)?,
        htotal: hactive.checked_add(hblank)?,
        vdisplay: vactive,
        vsync_start,
        vsync_end: vsync_start.checked_add(vsync_width)?,
        vtotal: vactive.checked_add(vblank)?,
        interlaced: descriptor[17] & 0x80 != 0,
    })
}

/// Builds the wire mode of one timing, named after its geometry.
///
/// The name carries the pixel size and the refresh the timing computes, which
/// is how a client tells two EDID modes of one panel apart.
pub(crate) fn mode(timing: EdidTiming, preferred: bool) -> DrmModeModeInfo {
    let mut name = [0u8; DRM_MODE_NAME_LEN];
    let mut cursor = write_decimal(&mut name, 0, u32::from(timing.hdisplay));
    if cursor < name.len() {
        name[cursor] = b'x';
        cursor += 1;
    }
    let _ = write_decimal(&mut name, cursor, u32::from(timing.vdisplay));
    // Refresh at 0.1 Hz resolution, rounded, the way `drm_mode_vrefresh`
    // reports it: clients see the number the timing really runs at.
    let pixels = u64::from(timing.htotal) * u64::from(timing.vtotal);
    let vrefresh = if pixels == 0 {
        0
    } else {
        ((u64::from(timing.clock) * 1_000 * 10).div_ceil(pixels) / 10) as u32
    };
    DrmModeModeInfo {
        clock: timing.clock,
        hdisplay: timing.hdisplay,
        hsync_start: timing.hsync_start,
        hsync_end: timing.hsync_end,
        htotal: timing.htotal,
        hskew: 0,
        vdisplay: timing.vdisplay,
        vsync_start: timing.vsync_start,
        vsync_end: timing.vsync_end,
        vtotal: timing.vtotal,
        vscan: 0,
        vrefresh,
        flags: u32::from(if timing.interlaced {
            DRM_MODE_FLAG_INTERLACE
        } else {
            0
        }),
        kind: u32::from(if preferred {
            DRM_MODE_TYPE_PREFERRED
        } else {
            0
        }),
        name,
    }
}
