// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Place a source `derive` after other safely movable outer attributes.

use rustc_ast::{AttrKind, AttrStyle, Attribute, Item, MetaItemInner, MetaItemKind};
use rustc_errors::{Applicability, Diag, DiagCtxtHandle, Diagnostic, Level};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext};
use rustc_span::Span;

rustc_session::declare_lint! {
    /// Reports a `derive` followed by safe built-in outer attributes.
    /// See `docs/code-guidelines/CG-007-attribute-order.md`.
    pub DERIVE_NOT_LAST,
    Deny,
    "derive should follow other safe outer attributes"
}

rustc_session::declare_lint_pass!(DeriveNotLast => [DERIVE_NOT_LAST]);

pub(crate) struct MoveDerive {
    pub(crate) span: Span,
    pub(crate) replacement: String,
}

impl<'a> Diagnostic<'a, ()> for MoveDerive {
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: Level) -> Diag<'a, ()> {
        let mut diag = Diag::new(dcx, level, "place derive after other outer attributes");
        diag.help("see docs/code-guidelines/CG-007-attribute-order.md");
        diag.span_suggestion(
            self.span,
            "move derive after these attributes",
            self.replacement,
            Applicability::MachineApplicable,
        );
        diag
    }
}

/// An intentionally narrow set of built-in attributes that can be moved past
/// `derive` without changing attribute macro expansion order.
pub(crate) fn safe_name(attr: &Attribute) -> Option<String> {
    if attr.style != AttrStyle::Outer || !matches!(attr.kind, AttrKind::Normal(..)) {
        return None;
    }
    let item = attr.get_normal_item();
    let [segment] = &*item.path.segments else {
        return None;
    };
    let name = segment.ident.name.as_str();
    if name == "cfg_attr" {
        let meta = attr.meta()?;
        let MetaItemKind::List(items) = meta.kind else {
            return None;
        };
        if items.len() < 2
            || !items[1..].iter().all(|item| {
                let MetaItemInner::MetaItem(meta) = item else {
                    return false;
                };
                let [segment] = &*meta.path.segments else {
                    return false;
                };
                matches!(
                    segment.ident.name.as_str(),
                    "cold" | "deprecated" | "inline" | "must_use" | "non_exhaustive"
                )
            })
        {
            return None;
        }
        return Some(name.to_string());
    }
    if matches!(
        name,
        "allow"
            | "cfg"
            | "cold"
            | "deny"
            | "deprecated"
            | "derive"
            | "expect"
            | "forbid"
            | "inline"
            | "link_section"
            | "must_use"
            | "non_exhaustive"
            | "repr"
            | "target_feature"
            | "used"
            | "warn"
    ) {
        Some(name.to_string())
    } else {
        None
    }
}

pub(crate) fn is_doc(attr: &Attribute) -> bool {
    matches!(attr.kind, AttrKind::DocComment(..))
        || attr.meta().is_some_and(|meta| {
            meta.path.segments.len() == 1 && meta.path.segments[0].ident.name.as_str() == "doc"
        })
}

/// Return snippets only when intervening text is whitespace. Comments can
/// carry intent and must remain attached to their original attribute.
pub(crate) fn snippets(
    cx: &EarlyContext<'_>,
    attrs: &[Attribute],
) -> Option<(Span, Vec<String>, Vec<String>)> {
    let source = cx.sess().source_map();
    let mut values = Vec::new();
    let mut gaps = Vec::new();
    for (index, attr) in attrs.iter().enumerate() {
        values.push(source.span_to_snippet(attr.span).ok()?);
        if let Some(next) = attrs.get(index + 1) {
            let gap = source.span_to_snippet(attr.span.between(next.span)).ok()?;
            if !gap.chars().all(char::is_whitespace) {
                return None;
            }
            gaps.push(gap);
        }
    }
    Some((attrs.first()?.span.to(attrs.last()?.span), values, gaps))
}

pub(crate) fn derive_movement(cx: &EarlyContext<'_>, item: &Item) -> Option<MoveDerive> {
    // Unknown attributes may be expansion-order-sensitive macros or derive
    // helpers. A cfg_attr with an unrecognized payload is also skipped.
    if item
        .attrs
        .iter()
        .any(|attr| !is_doc(attr) && safe_name(attr).is_none())
    {
        return None;
    }
    let names: Vec<_> = item.attrs.iter().map(safe_name).collect();
    let derive_index = names
        .iter()
        .position(|name| name.as_deref() == Some("derive"))?;
    if derive_index + 1 == names.len() || names[derive_index + 1..].iter().any(Option::is_none) {
        return None;
    }
    if names[derive_index + 1..]
        .iter()
        .any(|name| name.as_deref() == Some("derive"))
    {
        return None;
    }
    let attrs = &item.attrs[derive_index..];
    let (span, values, gaps) = snippets(cx, attrs)?;
    let mut replacement = String::new();
    for index in 1..values.len() {
        replacement.push_str(&values[index]);
        replacement.push_str(&gaps[index - 1]);
    }
    replacement.push_str(&values[0]);
    Some(MoveDerive { span, replacement })
}

impl EarlyLintPass for DeriveNotLast {
    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        if let Some(movement) = derive_movement(cx, item) {
            cx.emit_span_lint(DERIVE_NOT_LAST, movement.span, movement);
        }
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code)]

#[derive(Debug, Clone)]
#[repr(C)]
struct Flagged(u8);

#[repr(C)]
#[derive(Clone, Debug)]
struct AlreadyLast(u8);

#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, derive_not_last, reason = "CG-007-attribute-order: test suppression")]
#[derive(Debug, Clone)]
#[repr(C)]
struct Allowed(u8);

/// A doc barrier must not be crossed.
#[derive(Debug)]
/// Documentation in source order.
#[repr(C)]
struct Documented(u8);

#[derive(Debug)]
#[cfg_attr(any(), derive(Clone))]
struct Conditional(u8);

#[derive(Debug)]
#[cfg_attr(any(), custom_attribute)]
struct MacroAttribute(u8);

#[doc = "first"]
#[derive(Debug)]
#[doc = "second"]
#[repr(C)]
struct ExplicitDoc(u8);

#[derive(Debug)]
#[custom::doc]
#[repr(C)]
struct QualifiedDoc(u8);

fn main() {}
"#;

    const STDERR: &str = r#"error: place derive after other outer attributes
  --> $DIR/derive_not_last.rs:3:1
   |
LL | / #[derive(Debug, Clone)]
LL | | #[repr(C)]
   | |__________^
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md
   = note: `#[deny(derive_not_last)]` on by default
help: move derive after these attributes
   |
LL + #[repr(C)]
LL + #[derive(Debug, Clone)]
   |

error[E0433]: cannot find module or crate `custom` in this scope
  --> $DIR/derive_not_last.rs:38:3
   |
LL | #[custom::doc]
   |   ^^^^^^ use of unresolved module or unlinked crate `custom`

error: aborting due to 2 previous errors

For more information about this error, try `rustc --explain E0433`.
"#;

    #[test]
    fn ui() {
        crate::testing::run_ui("derive_not_last", SOURCE, STDERR, &[]);
    }
}
