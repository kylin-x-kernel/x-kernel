// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Allocation-free CPU mask/list text codec.
//!
//! The parser mirrors Linux v6.12 `bitmap_parse()` and
//! `bitmap_parselist()` for the accepted grammar. It additionally checks that
//! every accepted bit belongs to the caller-provided present set; Linux's
//! `nmaskbits` alone does not carry X-Kernel's present/past-boot distinction.
//! Formatting is canonical text only; callers add their own line ending.

use core::fmt::Write;

use crate::{KCpuMask, KCpuMaskExt, present_mask};

/// A validated, immutable CPU numbering domain.
///
/// Width is derived from the highest present bit so callers cannot construct a
/// domain whose width and membership disagree. The current X-Kernel model has
/// no independent possible-CPU set; `present` is the present-only limitation
/// documented by the design.
#[derive(Clone)]
pub struct CpuTextDomain {
    nr_cpu_ids: usize,
    present: KCpuMask,
}

impl CpuTextDomain {
    pub fn new(present: KCpuMask) -> Result<Self, CpuTextError> {
        let mut highest = None;
        for cpu in present.iter_logical() {
            highest = Some(cpu.as_usize());
        }
        let Some(highest) = highest else {
            return Err(CpuTextError::EmptyDomain);
        };
        Ok(Self {
            nr_cpu_ids: highest.checked_add(1).ok_or(CpuTextError::Overflow)?,
            present,
        })
    }

    /// Captures the current present map. A missing/empty map is an explicit
    /// failure; this never synthesizes CPU 0 or a stale domain.
    pub fn runtime() -> Result<Self, CpuTextError> {
        present_mask()
            .ok_or(CpuTextError::EmptyDomain)
            .and_then(Self::new)
    }

    pub const fn nr_cpu_ids(&self) -> usize {
        self.nr_cpu_ids
    }

    pub const fn present(&self) -> &KCpuMask {
        &self.present
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuTextError {
    InvalidSyntax,
    OutOfRange,
    Overflow,
    EmptyDomain,
    Output,
}

/// Parses a Linux-style comma-separated hexadecimal bitmap.
///
/// The parsed mask must contain only present CPUs. Short input leaves the
/// missing high chunks zero; a high chunk that sets a bit at or above
/// `nr_cpu_ids` returns `Overflow`, matching Linux's final `bitmap_parse()`
/// range check.
pub fn parse_bitmap(bytes: &[u8], domain: &CpuTextDomain) -> Result<KCpuMask, CpuTextError> {
    let text = input_bytes(bytes);
    let words = words(domain.nr_cpu_ids);
    let mut mask = KCpuMask::new();
    let mut end = text.len();
    let mut word = 0usize;
    let mut out_of_width = false;

    loop {
        while end > 0 && is_separator(text[end - 1]) {
            end -= 1;
        }
        if end == 0 {
            break;
        }
        let group_end = end;
        while end > 0 && !is_separator(text[end - 1]) {
            end -= 1;
        }
        let group = &text[end..group_end];
        if group.len() > 8 || word >= words {
            return Err(CpuTextError::Overflow);
        }
        let mut value = 0u32;
        for byte in group {
            value = (value << 4) | hex(*byte).ok_or(CpuTextError::InvalidSyntax)?;
        }
        let first = word * 32;
        for bit in 0..32 {
            let cpu = first + bit;
            if value & (1u32 << bit) == 0 {
                continue;
            }
            if cpu >= domain.nr_cpu_ids {
                out_of_width = true;
            } else {
                mask.set(cpu, true);
            }
        }
        word += 1;
    }
    if out_of_width {
        return Err(CpuTextError::Overflow);
    }
    validate_present(mask, domain)?;
    Ok(mask)
}

/// Parses a Linux-style CPU list with ranges, `N`, `all` and grouped ranges.
///
/// Membership is checked against `domain.present` after syntax expansion;
/// `all`/`N` are not silently intersected with the present set.
pub fn parse_list(bytes: &[u8], domain: &CpuTextDomain) -> Result<KCpuMask, CpuTextError> {
    let text = input_bytes(bytes);
    let mut cursor = Cursor { text, pos: 0 };
    let mut mask = KCpuMask::new();
    let nr_cpu_ids = domain.nr_cpu_ids;

    loop {
        cursor.skip_separators();
        if cursor.is_end() {
            validate_present(mask, domain)?;
            return Ok(mask);
        }
        let (start, end, off, group) = cursor.region(nr_cpu_ids)?;
        if start > end || group == 0 || off > group {
            return Err(CpuTextError::InvalidSyntax);
        }
        if end >= nr_cpu_ids {
            return Err(CpuTextError::OutOfRange);
        }
        let mut base = start;
        loop {
            let upper = core::cmp::min(
                base.checked_add(off).ok_or(CpuTextError::Overflow)?,
                end.saturating_add(1),
            );
            for cpu in base..upper {
                mask.set(cpu, true);
            }
            let Some(next) = base.checked_add(group) else {
                break;
            };
            if next <= base {
                break;
            }
            base = next;
            if base > end {
                break;
            }
        }
    }
}

/// Writes the canonical bitmap text without a trailing newline.
pub fn write_bitmap(
    mask: KCpuMask,
    domain: &CpuTextDomain,
    out: &mut impl Write,
) -> Result<(), CpuTextError> {
    let words = words(domain.nr_cpu_ids);
    for word in (0..words).rev() {
        let first = word * 32;
        let bits = core::cmp::min(32, domain.nr_cpu_ids - first);
        let mut value = 0u32;
        for bit in 0..bits {
            if mask.get(first + bit) {
                value |= 1u32 << bit;
            }
        }
        if word + 1 != words {
            out.write_char(',').map_err(|_| CpuTextError::Output)?;
        }
        let digits = if word + 1 == words {
            bits.div_ceil(4)
        } else {
            8
        };
        write!(out, "{value:0digits$x}").map_err(|_| CpuTextError::Output)?;
    }
    Ok(())
}

/// Writes the canonical compressed CPU list without a trailing newline.
pub fn write_list(
    mask: KCpuMask,
    domain: &CpuTextDomain,
    out: &mut impl Write,
) -> Result<(), CpuTextError> {
    let mut start = None;
    let mut wrote_range = false;
    for cpu in 0..domain.nr_cpu_ids {
        if mask.get(cpu) {
            start.get_or_insert(cpu);
        } else if let Some(first) = start.take() {
            write_separator(out, &mut wrote_range)?;
            write_range(out, first, cpu - 1)?;
        }
    }
    if let Some(first) = start {
        write_separator(out, &mut wrote_range)?;
        write_range(out, first, domain.nr_cpu_ids - 1)?;
    }
    Ok(())
}

fn write_separator(out: &mut impl Write, wrote: &mut bool) -> Result<(), CpuTextError> {
    if *wrote {
        out.write_char(',').map_err(|_| CpuTextError::Output)?;
    }
    *wrote = true;
    Ok(())
}

/// Maximum canonical bitmap text length, excluding a caller newline.
pub fn bitmap_capacity(domain: &CpuTextDomain) -> Result<usize, CpuTextError> {
    let words = words(domain.nr_cpu_ids);
    words
        .checked_mul(8)
        .and_then(|value| value.checked_add(words.saturating_sub(1)))
        .ok_or(CpuTextError::Overflow)
}

/// Safe maximum canonical list text length, excluding a caller newline.
pub fn list_capacity(domain: &CpuTextDomain) -> Result<usize, CpuTextError> {
    let digits = decimal_digits(domain.nr_cpu_ids.saturating_sub(1));
    domain
        .nr_cpu_ids
        .checked_mul(digits + 1)
        .ok_or(CpuTextError::Overflow)
}

fn validate_present(mask: KCpuMask, domain: &CpuTextDomain) -> Result<(), CpuTextError> {
    if mask
        .iter_logical()
        .any(|cpu| !domain.present.get(cpu.as_usize()))
    {
        return Err(CpuTextError::OutOfRange);
    }
    Ok(())
}

fn write_range(out: &mut impl Write, start: usize, end: usize) -> Result<(), CpuTextError> {
    if start == end {
        write!(out, "{start}").map_err(|_| CpuTextError::Output)
    } else {
        write!(out, "{start}-{end}").map_err(|_| CpuTextError::Output)
    }
}

fn decimal_digits(value: usize) -> usize {
    let mut digits = 1;
    let mut value = value;
    while value >= 10 {
        digits += 1;
        value /= 10;
    }
    digits
}

fn words(nr_cpu_ids: usize) -> usize {
    nr_cpu_ids.div_ceil(32)
}

fn input_bytes(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n' || *byte == 0)
        .unwrap_or(bytes.len());
    &bytes[..end]
}

fn is_separator(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\r' | b'\x0b' | b'\x0c' | b',')
}

fn hex(byte: u8) -> Option<u32> {
    match byte {
        b'0'..=b'9' => Some((byte - b'0') as u32),
        b'a'..=b'f' => Some((byte - b'a' + 10) as u32),
        b'A'..=b'F' => Some((byte - b'A' + 10) as u32),
        _ => None,
    }
}

struct Cursor<'a> {
    text: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn is_end(&self) -> bool {
        self.pos >= self.text.len()
    }

    fn byte(&self) -> Option<u8> {
        self.text.get(self.pos).copied()
    }

    fn skip_separators(&mut self) {
        while self.byte().is_some_and(is_separator) {
            self.pos += 1;
        }
    }

    fn region(&mut self, nr_cpu_ids: usize) -> Result<(usize, usize, usize, usize), CpuTextError> {
        let (start, end, allows_pattern) = if self.starts_with_ignore_ascii_case(b"all") {
            self.pos += 3;
            (0, nr_cpu_ids - 1, true)
        } else {
            let start = self.number(nr_cpu_ids)?;
            if self.byte() == Some(b'-') {
                self.pos += 1;
                (start, self.number(nr_cpu_ids)?, true)
            } else {
                (start, start, false)
            }
        };

        let (off, group) = if self.byte() == Some(b':') {
            if !allows_pattern {
                return Err(CpuTextError::InvalidSyntax);
            }
            self.pos += 1;
            let off = self.number(nr_cpu_ids)?;
            if self.byte() != Some(b'/') {
                return Err(CpuTextError::InvalidSyntax);
            }
            self.pos += 1;
            let group = self.number(nr_cpu_ids)?;
            (off, group)
        } else {
            (end + 1, end + 1)
        };

        if !self.is_end() && !self.byte().is_some_and(is_separator) {
            return Err(CpuTextError::InvalidSyntax);
        }
        Ok((start, end, off, group))
    }

    fn number(&mut self, nr_cpu_ids: usize) -> Result<usize, CpuTextError> {
        if self.byte() == Some(b'N') {
            self.pos += 1;
            return Ok(nr_cpu_ids - 1);
        }
        let start = self.pos;
        let mut value = 0u64;
        while let Some(byte) = self.byte() {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value
                .checked_mul(10)
                .and_then(|value| value.checked_add((byte - b'0') as u64))
                .ok_or(CpuTextError::Overflow)?;
            self.pos += 1;
        }
        if self.pos == start {
            return Err(CpuTextError::InvalidSyntax);
        }
        if value > u32::MAX as u64 {
            return Err(CpuTextError::Overflow);
        }
        usize::try_from(value).map_err(|_| CpuTextError::Overflow)
    }

    fn starts_with_ignore_ascii_case(&self, prefix: &[u8]) -> bool {
        self.text.len().saturating_sub(self.pos) >= prefix.len()
            && self.text[self.pos..self.pos + prefix.len()].eq_ignore_ascii_case(prefix)
    }
}
