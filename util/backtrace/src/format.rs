// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Raw frame formatting for host-side symbolication.
//!
//! The kernel prints raw instruction addresses in a stable, machine-parseable
//! format. The host side (`xkmake symbolize`) extracts the addresses from
//! panic/exception logs and resolves them against the unstripped
//! `kernel.debug.elf`:
//!
//! ```text
//! Backtrace:
//! 0: 0xffff000040123456
//! 1: 0xffff000040102abc
//! ```

use core::fmt;

use crate::frame::Frame;
#[cfg(feature = "symtab")]
use crate::symtab;

/// Format a list of raw frames.
///
/// Address-only output by default; with the `symtab` feature enabled each
/// line additionally carries a `func+0xoff/0xsize` annotation from the
/// compact kernel symbol table, while keeping the raw address first so the
/// host-side parser never depends on the symbol table.
pub(crate) fn fmt_frames(f: &mut fmt::Formatter<'_>, frames: &[Frame]) -> fmt::Result {
    writeln!(f, "Backtrace:")?;
    for (i, frame) in frames.iter().enumerate() {
        // Print the call-site address (`ip - 1` for ordinary frames) so the
        // host tool and the compact symbol table resolve the calling
        // instruction, not the instruction after the call.
        let ip = frame.adjust_ip();
        write!(f, "{}: {:#x}", i, ip)?;
        #[cfg(feature = "symtab")]
        symtab::write_annotation(f, ip)?;
        writeln!(f)?;
    }
    Ok(())
}
