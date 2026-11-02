// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal JSON-lines writer for facts records.
//!
//! The lint library has no JSON dependency; records are flat objects of
//! strings, integers, flags and pre-rendered arrays, so a small writer keeps
//! the library's dependency set unchanged.

use std::fmt::Write as _;

/// Appends `s` to `out` as a JSON string literal.
pub(super) fn push_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // Writing to a String cannot fail.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Renders `items` as a JSON array of strings.
pub(super) fn str_array<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    let mut out = String::from("[");
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_str(&mut out, item);
    }
    out.push(']');
    out
}

/// One record under construction; [`Record::end`] terminates the line.
pub(super) struct Record<'a> {
    out: &'a mut String,
}

impl<'a> Record<'a> {
    /// Starts a record whose `t` field is `tag`.
    pub(super) fn new(out: &'a mut String, tag: &str) -> Self {
        out.push_str("{\"t\":");
        push_str(out, tag);
        Self { out }
    }

    fn key(&mut self, key: &str) {
        self.out.push(',');
        push_str(self.out, key);
        self.out.push(':');
    }

    pub(super) fn str(mut self, key: &str, value: &str) -> Self {
        self.key(key);
        push_str(self.out, value);
        self
    }

    /// Writes `key` only when `value` is present.
    pub(super) fn opt(self, key: &str, value: Option<&str>) -> Self {
        match value {
            Some(value) => self.str(key, value),
            None => self,
        }
    }

    pub(super) fn num(mut self, key: &str, value: u64) -> Self {
        self.key(key);
        // Writing to a String cannot fail.
        let _ = write!(self.out, "{value}");
        self
    }

    /// Writes `key: true` when `value` holds; absent means false.
    pub(super) fn flag(self, key: &str, value: bool) -> Self {
        if value { self.raw(key, "true") } else { self }
    }

    /// Writes an already rendered JSON value.
    pub(super) fn raw(mut self, key: &str, value: &str) -> Self {
        self.key(key);
        self.out.push_str(value);
        self
    }

    pub(super) fn end(self) {
        self.out.push_str("}\n");
    }
}

#[cfg(test)]
mod tests {
    use super::{Record, push_str, str_array};

    #[test]
    fn escapes_json_strings() {
        let mut out = String::new();
        push_str(&mut out, "a\"b\\c\n\u{1}");
        assert_eq!(out, "\"a\\\"b\\\\c\\n\\u0001\"");
    }

    #[test]
    fn writes_one_record_per_line() {
        let mut out = String::new();
        Record::new(&mut out, "call")
            .str("caller", "k::f")
            .opt("macro", None)
            .num("line", 3)
            .flag("generic", false)
            .flag("inlined", true)
            .raw("orderings", &str_array(["Acquire", "Relaxed"]))
            .end();
        assert_eq!(
            out,
            "{\"t\":\"call\",\"caller\":\"k::f\",\"line\":3,\"inlined\":true,\"orderings\":[\"\
             Acquire\",\"Relaxed\"]}\n"
        );
    }
}
