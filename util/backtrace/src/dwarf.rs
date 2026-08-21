// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::borrow::Cow;
use core::{cell::UnsafeCell, fmt, slice};

use addr2line::Context;
use klazy::Once;
// Only import in non-test builds
#[cfg(not(any(test, unittest)))]
use log::{error, info, warn};
#[cfg(not(any(test, unittest)))]
use paste::paste;

pub type DwarfReader = gimli::EndianSlice<'static, gimli::RunTimeEndian>;

struct DwarfContextStorage(UnsafeCell<Option<Context<DwarfReader>>>);

impl DwarfContextStorage {
    const fn new() -> Self {
        Self(UnsafeCell::new(None))
    }

    fn get(&self) -> Option<&Context<DwarfReader>> {
        // SAFETY: the context is written at most once during `init()`, after
        // which it is read-only. Callers only borrow it immutably.
        unsafe { (*self.0.get()).as_ref() }
    }

    #[cfg_attr(any(test, unittest), allow(dead_code))]
    fn init_once(&self, ctx: Context<DwarfReader>) {
        // SAFETY: `INIT_ONCE` ensures this write runs at most once before the
        // context is ever observed. Subsequent access is read-only.
        unsafe { *self.0.get() = Some(ctx) };
    }
}

// SAFETY: initialization happens once under `INIT_ONCE`; afterwards the stored
// context is treated as immutable shared state through `get()`.
unsafe impl Sync for DwarfContextStorage {}

#[cfg_attr(any(test, unittest), allow(dead_code))]
static CONTEXT: DwarfContextStorage = DwarfContextStorage::new();
#[cfg_attr(any(test, unittest), allow(dead_code))]
static INIT_ONCE: Once<()> = Once::new();

// Only define macro in non-test builds
#[cfg(not(any(test, unittest)))]
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
                let $name = {
                    // SAFETY: The linker provides matching `__start_*`/`__stop_*`
                    // symbols for each DWARF section, so this slice spans the
                    // exact bytes of the embedded section.
                    let section = unsafe {
                        core::slice::from_raw_parts(
                            [<__start_ $name>].as_ptr(),
                            [<__stop_ $name>]
                                .as_ptr()
                                .offset_from_unsigned([<__start_ $name>].as_ptr()),
                        )
                    };
                    DwarfReader::new(section, gimli::RunTimeEndian::default())
                };
            )*
        }
    };
}

// Stub macro for test builds - does nothing
#[cfg(any(test, unittest))]
#[allow(unused_macros)] // Intentionally unused in tests
macro_rules! generate_sections {
    ($($name:ident),*) => {
        // No-op in test mode
    };
}

/// Returns whether the in-kernel DWARF context is initialized and ready.
///
/// The [`Backtrace`](crate::Backtrace) display impl falls back to raw
/// address output when this is false.
pub(crate) fn is_ready() -> bool {
    CONTEXT.get().is_some()
}

#[cfg_attr(any(test, unittest), allow(dead_code))]
pub fn init() {
    INIT_ONCE.call_once(|| {
        // Only initialize DWARF in kernel builds
        #[cfg(not(any(test, unittest)))]
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

            let try_init = |debug_addr: DwarfReader,
                            debug_aranges: DwarfReader,
                            debug_rnglists: DwarfReader,
                            debug_str_offsets: DwarfReader|
             -> Result<Context<DwarfReader>, gimli::Error> {
                Context::from_sections(
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
                )
            };

            let mut init_result =
                try_init(debug_addr, debug_aranges, debug_rnglists, debug_str_offsets);
            let mut degraded = false;

            if let Err(e) = &init_result {
                warn!(
                    "DWARF init failed with full sections ({e}); sizes: abbrev={:#x} addr={:#x} \
                     aranges={:#x} info={:#x} line={:#x} line_str={:#x} ranges={:#x} \
                     rnglists={:#x} str={:#x} str_offsets={:#x}",
                    debug_abbrev.len(),
                    debug_addr.len(),
                    debug_aranges.len(),
                    debug_info.len(),
                    debug_line.len(),
                    debug_line_str.len(),
                    debug_ranges.len(),
                    debug_rnglists.len(),
                    debug_str.len(),
                    debug_str_offsets.len(),
                );
                init_result = try_init(
                    default_section,
                    default_section,
                    default_section,
                    default_section,
                );
                degraded = true;
            }

            match init_result {
                Ok(ctx) => {
                    CONTEXT.init_once(ctx);
                    if degraded {
                        warn!(
                            "Initialized addr2line context after ignoring optional DWARF sections."
                        );
                    } else {
                        info!("Initialized addr2line context successfully.");
                    }
                }
                Err(e) => {
                    error!("Failed to initialize addr2line context: {e}");
                }
            }
        }

        // Skip DWARF initialization in test mode
        #[cfg(any(test, unittest))]
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
        let ctx = CONTEXT.get()?;

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

#[cfg(not(any(test, unittest)))]
pub(crate) fn fmt_frames(f: &mut fmt::Formatter<'_>, frames: &[crate::Frame]) -> fmt::Result {
    if frames.is_empty() {
        writeln!(f, "  <no frames captured>")?;
        return Ok(());
    }

    if CONTEXT.get().is_none() {
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
            writeln!(f, "Backtracing is not initialized. Raw frames:")?;
            for (i, frame) in frames.iter().enumerate() {
                writeln!(f, "  {i:>4}: {frame}")?;
            }
            return Ok(());
        }
    }

    let ctx = CONTEXT
        .get()
        .expect("checked above that DWARF context is initialized");

    // Symbolicate each raw frame individually and always preserve a raw fallback.
    for (i, raw) in frames.iter().enumerate() {
        let mut symbolized = false;

        for ip in [raw.adjust_ip(), raw.ip] {
            let Some(mut iter) = ctx.find_frames(ip as _).skip_all_loads().ok() else {
                continue;
            };

            while let Ok(Some(frame)) = iter.next() {
                symbolized = true;
                write!(f, "{i:>4}")?;
                fmt_frame(f, &frame)?;
                writeln!(f, " with {raw}")?;
            }

            if symbolized {
                break;
            }
        }

        if !symbolized {
            writeln!(f, "{i:>4}: <no DWARF symbol> with {raw}")?;
        }
    }

    Ok(())
}

#[cfg(any(test, unittest))]
pub(crate) fn fmt_frames(f: &mut fmt::Formatter<'_>, frames: &[crate::Frame]) -> fmt::Result {
    if CONTEXT.get().is_none() {
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

#[cfg(unittest)]
mod tests_unittest {
    use alloc::{format, string::String};
    use core::fmt;

    use unittest::def_test;

    use super::*;

    struct DisplayOneFrame(addr2line::Frame<'static, DwarfReader>);

    impl fmt::Display for DisplayOneFrame {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_frame(f, &self.0)
        }
    }

    struct DisplayFrames<'a>(&'a [crate::Frame]);

    impl fmt::Display for DisplayFrames<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            fmt_frames(f, self.0)
        }
    }

    fn render_symbolized_frame(frame: addr2line::Frame<'static, DwarfReader>) -> String {
        format!("{}", DisplayOneFrame(frame))
    }

    fn plain_function_frame() -> addr2line::Frame<'static, DwarfReader> {
        addr2line::Frame {
            dw_die_offset: None,
            function: Some(addr2line::FunctionName {
                name: DwarfReader::new(b"plain_function", gimli::RunTimeEndian::default()),
                language: None,
            }),
            location: None,
        }
    }

    #[def_test]
    fn dwarf_context_storage_starts_empty() {
        let storage = DwarfContextStorage::new();
        assert!(storage.get().is_none());
    }

    #[def_test]
    fn frame_iter_returns_none_without_context() {
        let frames = [crate::Frame::new(0x1000, 0x2000)];
        let mut iter = FrameIter::new(&frames);

        assert!(iter.next().is_none());
        assert!(iter.next().is_none());
    }

    #[def_test]
    fn fmt_frame_falls_back_to_unknown_function_without_location() {
        let rendered = render_symbolized_frame(addr2line::Frame {
            dw_die_offset: None,
            function: None,
            location: None,
        });

        assert_eq!(rendered, ": <unknown>\n");
    }

    #[def_test]
    fn fmt_frame_stops_at_unknown_file_boundary() {
        let mut frame = plain_function_frame();
        frame.location = Some(addr2line::Location {
            file: None,
            line: Some(42),
            column: Some(7),
        });

        let rendered = render_symbolized_frame(frame);

        assert_eq!(rendered, ": plain_function\n            at ??");
    }

    #[def_test]
    fn fmt_frame_includes_full_source_location_when_available() {
        let mut frame = plain_function_frame();
        frame.location = Some(addr2line::Location {
            file: Some("src/dwarf.rs"),
            line: Some(42),
            column: Some(7),
        });

        let rendered = render_symbolized_frame(frame);

        assert_eq!(
            rendered,
            ": plain_function\n            at src/dwarf.rs:42:7"
        );
    }

    #[def_test]
    fn fmt_frames_reports_empty_capture() {
        assert_eq!(
            format!("{}", DisplayFrames(&[])),
            "Symbolication disabled in test mode.\nRaw frames:\n"
        );
    }

    #[def_test]
    fn fmt_frames_without_context_reports_raw_frames() {
        let frames = [crate::Frame::new(0x10, 0x20), crate::Frame::new(0x30, 0x40)];

        let rendered = format!("{}", DisplayFrames(&frames));

        assert!(rendered.starts_with("Symbolication disabled in test mode.\nRaw frames:\n"));
        assert!(rendered.contains("     0: fp=0x0000000000000010, ip=0x0000000000000020"));
        assert!(rendered.contains("     1: fp=0x0000000000000030, ip=0x0000000000000040"));
    }
}
