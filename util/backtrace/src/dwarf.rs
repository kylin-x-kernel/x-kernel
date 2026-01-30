// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::borrow::Cow;
use core::{fmt, slice};

use addr2line::Context;
// Only import in non-test builds
#[cfg(not(test))]
use log::{error, info};
#[cfg(not(test))]
use paste::paste;
use spin::Once;

pub type DwarfReader = gimli::EndianSlice<'static, gimli::RunTimeEndian>;

static mut CONTEXT: Option<Context<DwarfReader>> = None;

#[cfg_attr(test, allow(dead_code))]
static INIT_ONCE: Once<()> = Once::new();

// Only define macro in non-test builds
#[cfg(not(test))]
#[allow(unused_macros)] // Used at runtime via macro expansion
macro_rules! generate_sections {
    ($($name:ident),*) => {
        unsafe extern "C" {
            paste! {
                $(
                    safe static [<__start_ $name>]: [u8; 0];
                    safe static [<__stop_ $name>]: [u8; 0];
                )*
            }
        }

        paste! {
            $(
                let $name = DwarfReader::new(
                    unsafe {
                        core::slice::from_raw_parts(
                            [<__start_ $name>].as_ptr(),
                            [<__stop_ $name>]
                                .as_ptr()
                                .offset_from_unsigned([<__start_ $name>].as_ptr()),
                        )
                    },
                    gimli::RunTimeEndian::default(),
                );
            )*
        }
    };
}

// Stub macro for test builds - does nothing
#[cfg(test)]
#[allow(unused_macros)] // Intentionally unused in tests
macro_rules! generate_sections {
    ($($name:ident),*) => {
        // No-op in test mode
    };
}

#[cfg_attr(test, allow(dead_code))]
pub fn init() {
    INIT_ONCE.call_once(|| {
        // Only initialize DWARF in kernel builds
        #[cfg(not(test))]
        {
            generate_sections!(
                debug_abbrev,
                debug_addr,
                debug_aranges,
                debug_info,
                debug_line,
                debug_line_str,
                debug_ranges,
                debug_rnglists,
                debug_str,
                debug_str_offsets
            );

            let default_section = DwarfReader::new(&[], gimli::RunTimeEndian::default());

            match Context::from_sections(
                debug_abbrev.into(),
                debug_addr.into(),
                debug_aranges.into(),
                debug_info.into(),
                debug_line.into(),
                debug_line_str.into(),
                debug_ranges.into(),
                debug_rnglists.into(),
                debug_str.into(),
                debug_str_offsets.into(),
                default_section,
            ) {
                Ok(ctx) => {
                    unsafe {
                        CONTEXT = Some(ctx);
                    }
                    info!("Initialized addr2line context successfully.");
                }
                Err(e) => {
                    error!("Failed to initialize addr2line context: {e}");
                }
            }
        }

        // Skip DWARF initialization in test mode
        #[cfg(test)]
        {
            // DWARF initialization is skipped in test builds because external
            // symbols (__start_debug_*, __stop_debug_*) are only available in
            // kernel builds with the appropriate linker script.
        }
    });
}

/// An iterator over the stack frames in a captured backtrace.
///
/// See [`Backtrace::frames`].
///
/// [`Backtrace::frames`]: crate::Backtrace::frames
pub struct FrameIter<'a> {
    src: slice::Iter<'a, crate::Frame>,
    inner: Option<(crate::Frame, addr2line::FrameIter<'static, DwarfReader>)>,
}

impl<'a> FrameIter<'a> {
    pub(crate) fn new(frames: &'a [crate::Frame]) -> Self {
        let src = frames.iter();
        Self { src, inner: None }
    }
}

impl Iterator for FrameIter<'_> {
    type Item = (crate::Frame, addr2line::Frame<'static, DwarfReader>);

    fn next(&mut self) -> Option<Self::Item> {
        #[allow(static_mut_refs)]
        let ctx = unsafe { CONTEXT.as_ref()? };

        loop {
            if let Some((raw, inner)) = &mut self.inner
                && let Ok(Some(frame)) = inner.next()
            {
                return Some((*raw, frame));
            }

            let raw = self.src.next()?;
            self.inner = ctx
                .find_frames(raw.adjust_ip() as _)
                .skip_all_loads()
                .ok()
                .map(|x| (*raw, x));
        }
    }
}

fn fmt_frame<R: gimli::Reader>(
    f: &mut fmt::Formatter<'_>,
    frame: &addr2line::Frame<R>,
) -> fmt::Result {
    let func = frame
        .function
        .as_ref()
        .and_then(|func| func.demangle().ok())
        .unwrap_or(Cow::Borrowed("<unknown>"));
    writeln!(f, ": {func}")?;

    let Some(location) = &frame.location else {
        return Ok(());
    };
    write!(f, "            at ")?;

    let Some(file) = &location.file else {
        return write!(f, "??");
    };
    write!(f, "{file}")?;
    let Some(line) = location.line else {
        return Ok(());
    };
    write!(f, ":{line}")?;
    let Some(col) = location.column else {
        return Ok(());
    };
    write!(f, ":{col}")?;

    Ok(())
}

#[cfg(not(test))]
pub(crate) fn fmt_frames(f: &mut fmt::Formatter<'_>, frames: &[crate::Frame]) -> fmt::Result {
    #[allow(static_mut_refs)]
    if unsafe { CONTEXT.is_none() } {
        // In test mode, symbolication is not available
        #[cfg(test)]
        {
            writeln!(f, "Symbolication not available in test mode.")?;
            writeln!(f, "Raw frames:")?;
            for (i, frame) in frames.iter().enumerate() {
                writeln!(f, "  {:>4}: {}", i, frame)?;
            }
            return Ok(());
        }

        // In kernel mode, this is an error
        #[cfg(not(test))]
        {
            return write!(f, "Backtracing is not initialized.");
        }
    }

    // Normal symbolication
    for (i, (raw, frame)) in FrameIter::new(frames).enumerate() {
        write!(f, "{i:>4}")?;
        fmt_frame(f, &frame)?;
        writeln!(f, " with {raw}")?;
    }

    Ok(())
}

#[cfg(test)]
pub(crate) fn fmt_frames(f: &mut fmt::Formatter<'_>, frames: &[crate::Frame]) -> fmt::Result {
    #[allow(static_mut_refs)]
    if unsafe { CONTEXT.is_none() } {
        writeln!(f, "Symbolication disabled in test mode.")?;
        writeln!(f, "Raw frames:")?;
        for (i, frame) in frames.iter().enumerate() {
            writeln!(f, "  {:>4}: {}", i, frame)?;
        }
        return Ok(());
    }

    // 正常的符号化输出
    for (i, (raw, frame)) in FrameIter::new(frames).enumerate() {
        write!(f, "{i:>4}")?;
        fmt_frame(f, &frame)?;
        writeln!(f, " with {raw}")?;
    }
    Ok(())
}
