// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Presence of a local safety argument for unsafe symbol and naked attributes.
//!
//! Rule: `docs/code-guidelines/CG-008-unsafe-attribute-safety.md`.

use rustc_ast::{AssocItem, AttrKind, Attribute, Item, ItemKind, Safety};
use rustc_errors::{Diag, DiagCtxtHandle, Diagnostic, Level, msg};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_span::{Span, sym};

rustc_session::declare_lint! {
    /// Reports `#[unsafe(naked)]`, `#[unsafe(no_mangle)]`, and
    /// `#[unsafe(export_name = ..)]` without a preceding `// SAFETY:` comment.
    pub UNDOCUMENTED_UNSAFE_ATTRIBUTE,
    Deny,
    "unsafe attribute has no preceding SAFETY comment"
}

rustc_session::declare_lint_pass!(UndocumentedUnsafeAttribute => [UNDOCUMENTED_UNSAFE_ATTRIBUTE]);

struct MissingSafety {
    span: Span,
}

impl<'a> Diagnostic<'a, ()> for MissingSafety {
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: Level) -> Diag<'a, ()> {
        Diag::new(
            dcx,
            level,
            msg!("unsafe attribute has no preceding SAFETY comment"),
        )
        .with_span(self.span)
        .with_help(msg!(
            "add a // SAFETY: justification above the attribute group; see \
             docs/code-guidelines/CG-008-unsafe-attribute-safety.md"
        ))
    }
}

fn is_scoped_attribute(attribute: &Attribute) -> bool {
    let AttrKind::Normal(normal) = &attribute.kind else {
        return false;
    };
    let item = &normal.item;
    matches!(item.unsafety, Safety::Unsafe(_))
        && (item.path == sym::naked || item.path == sym::no_mangle || item.path == sym::export_name)
}

fn source_between(cx: &EarlyContext<'_>, from: Span, to: Span) -> Option<String> {
    let map = cx.sess().source_map();
    (map.lookup_source_file(from.lo()).start_pos == map.lookup_source_file(to.lo()).start_pos)
        .then(|| {
            map.span_to_snippet(from.with_lo(from.hi()).with_hi(to.lo()))
                .ok()
        })
        .flatten()
}

fn group_start<'a>(cx: &EarlyContext<'_>, attrs: &'a [Attribute], index: usize) -> &'a Attribute {
    let mut start = index;
    while start > 0 {
        let Some(gap) = source_between(cx, attrs[start - 1].span, attrs[start].span) else {
            break;
        };
        if !gap.chars().all(char::is_whitespace)
            || gap.bytes().filter(|byte| *byte == b'\n').count() > 1
        {
            break;
        }
        start -= 1;
    }
    &attrs[start]
}

fn has_safety_comment(cx: &EarlyContext<'_>, attribute: &Attribute) -> bool {
    let map = cx.sess().source_map();
    let file = map.lookup_source_file(attribute.span.lo());
    let before = attribute
        .span
        .with_lo(file.start_pos)
        .with_hi(attribute.span.lo());
    let Ok(source) = map.span_to_snippet(before) else {
        return false;
    };
    // Ignore indentation on the attribute's own line, then inspect only the
    // contiguous comment immediately above it.
    let Some((previous_lines, own_line)) = source.rsplit_once('\n') else {
        return false;
    };
    if !own_line.trim().is_empty() {
        return false;
    }
    previous_lines
        .split('\n')
        .rev()
        .take_while(|line| {
            let line = line.trim_start();
            !line.is_empty() && line.starts_with("//")
        })
        .any(|line| line.trim_start().starts_with("// SAFETY:"))
}

fn check_attributes(cx: &EarlyContext<'_>, attrs: &[Attribute]) {
    for (index, attribute) in attrs.iter().enumerate() {
        if !is_scoped_attribute(attribute) || has_safety_comment(cx, group_start(cx, attrs, index))
        {
            continue;
        }
        cx.emit_span_lint(
            UNDOCUMENTED_UNSAFE_ATTRIBUTE,
            attribute.span,
            MissingSafety {
                span: attribute.span,
            },
        );
    }
}

impl EarlyLintPass for UndocumentedUnsafeAttribute {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        check_attributes(cx, &item.attrs);
        if let ItemKind::ForeignMod(module) = &item.kind {
            for foreign_item in &module.items {
                check_attributes(cx, &foreign_item.attrs);
            }
        }
    }

    fn check_impl_item(&mut self, cx: &EarlyContext<'_>, item: &AssocItem) {
        check_attributes(cx, &item.attrs);
    }

    fn check_trait_item(&mut self, cx: &EarlyContext<'_>, item: &AssocItem) {
        check_attributes(cx, &item.attrs);
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code, unused_attributes)]

#[unsafe(no_mangle)]
pub extern "C" fn missing_name() {}

#[unsafe(export_name = "missing_export")]
pub extern "C" fn exported() {}

#[unsafe(naked)]
pub unsafe extern "C" fn naked() {
    core::arch::naked_asm!("ret");
}

// SAFETY: the unique symbol is consumed by a C ABI caller with no arguments.
#[unsafe(no_mangle)]
pub extern "C" fn documented() {}

// SAFETY: the assembler enters with a valid stack; this body contains only asm.
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grouped() {
    core::arch::naked_asm!("ret");
}

#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, undocumented_unsafe_attribute, reason = "CG-008-unsafe-attribute-safety: generated external contract")]
#[unsafe(no_mangle)]
pub extern "C" fn allowed() {}

#[cfg(any())]
#[unsafe(no_mangle)]
pub extern "C" fn cfg_disabled() {}

macro_rules! generated {
    ($name:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name() {}
    }
}
generated!(macro_generated);

#[cfg_attr(all(), unsafe(no_mangle))]
pub extern "C" fn cfg_attr_nested() {}

#[unsafe(link_section = ".test")]
pub static LINK_SECTION_ONLY: u8 = 0;

mod indented {
    // SAFETY: this test owns the unique documented_nested symbol.
    #[unsafe(no_mangle)]
    pub extern "C" fn documented_nested() {}

    #[unsafe(/* comment */ no_mangle)]
    pub extern "C" fn commented_attribute() {}
}

// SAFETY: the following inline function has a unique C ABI symbol.
#[inline(never)]

#[unsafe(no_mangle)]
pub extern "C" fn blank_group() {}

struct Provider;
impl Provider {
    #[unsafe(no_mangle)]
    pub extern "C" fn missing_associated() {}

    // SAFETY: this test owns the unique documented_associated C ABI symbol.
    #[unsafe(no_mangle)]
    pub extern "C" fn documented_associated() {}

    #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
    #[allow(unknown_lints, undocumented_unsafe_attribute, reason = "CG-008-unsafe-attribute-safety: test allow")]
    #[unsafe(no_mangle)]
    pub extern "C" fn allowed_associated() {}
}

trait Interface {
    #[unsafe(no_mangle)]
    fn missing_trait_method();
}

unsafe extern "C" {
    #[unsafe(no_mangle)]
    fn missing_foreign();
}

#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, undocumented_unsafe_attribute, reason = "CG-008-unsafe-attribute-safety: test allow")]
unsafe extern "C" {
    #[unsafe(no_mangle)]
    fn allowed_foreign();
}

#[inline(never)]
fn inner_attribute() {
    #![unsafe(no_mangle)]
}

// SAFETY: this test owns the unique zero_blank C ABI symbol.
#[unsafe(no_mangle)]
pub extern "C" fn zero_blank() {}

// SAFETY: this test owns the unique one_blank C ABI symbol.

#[unsafe(no_mangle)]
pub extern "C" fn one_blank() {}

// SAFETY: this test owns the unique two_blank C ABI symbol.


#[unsafe(no_mangle)]
pub extern "C" fn two_blank() {}

fn main() {}
"#;

    const STDERR: &str = r#"error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:3:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md
   = note: `#[deny(undocumented_unsafe_attribute)]` on by default

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:6:1
   |
LL | #[unsafe(export_name = "missing_export")]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:9:1
   |
LL | #[unsafe(naked)]
   | ^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:31:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:53:5
   |
LL |     #[unsafe(/* comment */ no_mangle)]
   |     ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:60:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:65:5
   |
LL |     #[unsafe(no_mangle)]
   |     ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:79:5
   |
LL |     #[unsafe(no_mangle)]
   |     ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:84:5
   |
LL |     #[unsafe(no_mangle)]
   |     ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:97:5
   |
LL |     #![unsafe(no_mangle)]
   |     ^^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:106:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: unsafe attribute has no preceding SAFETY comment
  --> $DIR/undocumented_unsafe_attribute.rs:112:1
   |
LL | #[unsafe(no_mangle)]
   | ^^^^^^^^^^^^^^^^^^^^
   |
   = help: add a // SAFETY: justification above the attribute group; see docs/code-guidelines/CG-008-unsafe-attribute-safety.md

error: aborting due to 12 previous errors

"#;

    #[test]
    fn ui() {
        crate::testing::run_ui("undocumented_unsafe_attribute", SOURCE, STDERR, &[]);
    }
}
