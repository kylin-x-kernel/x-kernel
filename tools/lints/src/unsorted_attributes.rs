// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Alphabetize safe source attributes and built-in derive traits.

use std::collections::HashSet;

use rustc_ast::{
    Attribute, Block, Item, ItemKind, MetaItemInner, MetaItemKind, ModKind, StmtKind, UseTree,
    UseTreeKind,
};
use rustc_errors::{Applicability, Diag, DiagCtxtHandle, Diagnostic, Level};
use rustc_lint::{EarlyContext, EarlyLintPass, LintContext, LintPass};
use rustc_span::Span;

use crate::derive_not_last::{derive_movement, is_doc, safe_name, snippets};

rustc_session::declare_lint! {
    /// Reports safely reorderable outer attributes or built-in derive traits
    /// that are not alphabetized. See CG-007-attribute-order.
    pub UNSORTED_ATTRIBUTES,
    Deny,
    "safe attributes or built-in derives are not alphabetical"
}

pub(crate) struct UnsortedAttributes {
    imports: Vec<HashSet<String>>,
    block_imports: Vec<(Span, HashSet<String>)>,
}

impl UnsortedAttributes {
    pub(crate) fn new() -> Self {
        Self {
            imports: Vec::new(),
            block_imports: Vec::new(),
        }
    }
}

impl LintPass for UnsortedAttributes {
    fn name(&self) -> &'static str {
        "UnsortedAttributes"
    }

    fn get_lints(&self) -> rustc_lint::LintVec {
        vec![UNSORTED_ATTRIBUTES]
    }
}

struct SortAttributes {
    span: Span,
    replacement: Option<String>,
    subject: &'static str,
}

impl<'a> Diagnostic<'a, ()> for SortAttributes {
    fn into_diag(self, dcx: DiagCtxtHandle<'a>, level: Level) -> Diag<'a, ()> {
        let mut diag = Diag::new(dcx, level, format!("sort {} alphabetically", self.subject));
        diag.help("see docs/code-guidelines/CG-007-attribute-order.md");
        if let Some(replacement) = self.replacement {
            diag.span_suggestion(
                self.span,
                "use alphabetical order",
                replacement,
                Applicability::MachineApplicable,
            );
        }
        diag
    }
}

fn is_sortable(name: &str) -> bool {
    matches!(
        name,
        "cfg"
            | "cfg_attr"
            | "cold"
            | "deprecated"
            | "inline"
            | "link_section"
            | "must_use"
            | "non_exhaustive"
            | "repr"
            | "target_feature"
            | "used"
    )
}

fn effects(attr: &Attribute, name: &str) -> Option<Vec<String>> {
    if name != "cfg_attr" {
        return Some(vec![name.to_owned()]);
    }
    let meta = attr.meta()?;
    let MetaItemKind::List(items) = meta.kind else {
        return None;
    };
    items[1..]
        .iter()
        .map(|item| {
            let MetaItemInner::MetaItem(meta) = item else {
                return None;
            };
            Some(meta.path.segments.first()?.ident.name.as_str().to_owned())
        })
        .collect()
}

fn conflicts(a: &str, b: &str) -> bool {
    a == b
        || (["inline", "cold", "target_feature"].contains(&a)
            && ["inline", "cold", "target_feature"].contains(&b))
}

fn independent(attrs: &[Attribute], names: &[String]) -> bool {
    let mut seen: Vec<String> = Vec::new();
    for (attr, name) in attrs.iter().zip(names) {
        let Some(current) = effects(attr, name) else {
            return false;
        };
        for effect in &current {
            if seen.iter().any(|previous| conflicts(previous, effect)) {
                return false;
            }
        }
        seen.extend(current);
    }
    true
}

fn sort_attribute_run(cx: &EarlyContext<'_>, attrs: &[Attribute], names: &[String]) {
    if attrs.len() < 2 {
        return;
    }
    if !independent(attrs, names) {
        return;
    }
    let Some((span, values, gaps)) = snippets(cx, attrs) else {
        return;
    };
    let mut indices: Vec<_> = (0..attrs.len()).collect();
    indices.sort_by(|&a, &b| {
        names[a].cmp(&names[b]).then_with(|| {
            if names[a] == "cfg_attr" {
                values[a].cmp(&values[b])
            } else {
                std::cmp::Ordering::Equal
            }
        })
    });
    if indices
        .iter()
        .enumerate()
        .all(|(position, index)| position == *index)
    {
        return;
    }
    let mut replacement = String::new();
    for (position, index) in indices.into_iter().enumerate() {
        if position != 0 {
            replacement.push_str(&gaps[position - 1]);
        }
        replacement.push_str(&values[index]);
    }
    cx.emit_span_lint(
        UNSORTED_ATTRIBUTES,
        span,
        SortAttributes {
            span,
            replacement: Some(replacement),
            subject: "attributes",
        },
    );
}

fn builtin_derive(path: &rustc_ast::Path) -> Option<String> {
    let segments = &*path.segments;
    let name = segments.last()?.ident.name.as_str();
    let module = match name {
        "Clone" => "clone",
        "Copy" => "marker",
        "Debug" => "fmt",
        "Default" => "default",
        "Eq" | "Ord" | "PartialEq" | "PartialOrd" => "cmp",
        "Hash" => "hash",
        _ => return None,
    };
    match segments {
        [_] => Some(name.to_owned()),
        [root, middle, _]
            if matches!(root.ident.name.as_str(), "core" | "std")
                && middle.ident.name.as_str() == module =>
        {
            Some(name.to_owned())
        }
        _ => None,
    }
}

fn collect_use(tree: &UseTree, imports: &mut HashSet<String>) {
    match &tree.kind {
        UseTreeKind::Simple(_) => {
            imports.insert(tree.ident().name.as_str().to_owned());
        }
        UseTreeKind::Nested { items, .. } => {
            for (tree, _) in items {
                collect_use(tree, imports);
            }
        }
        UseTreeKind::Glob => {
            imports.insert("*".into());
        }
    }
}

fn record_import(item: &Item, imports: &mut HashSet<String>) {
    match &item.kind {
        ItemKind::Use(tree) => collect_use(tree, imports),
        ItemKind::ExternCrate(..)
            if item.attrs.iter().any(|attr| {
                attr.meta().is_some_and(|meta| {
                    meta.path.segments.len() == 1
                        && meta.path.segments[0].ident.name.as_str() == "macro_use"
                })
            }) =>
        {
            imports.insert("*".into());
        }
        _ => {}
    }
}

fn module_imports<'a>(items: impl IntoIterator<Item = &'a Box<Item>>) -> HashSet<String> {
    let mut imports = HashSet::new();
    for item in items {
        record_import(item, &mut imports);
    }
    imports
}

fn sort_derive(cx: &EarlyContext<'_>, attr: &Attribute, imports: &[&HashSet<String>]) {
    let Some(meta) = attr.meta() else { return };
    let MetaItemKind::List(traits) = meta.kind else {
        return;
    };
    if traits.len() < 2 {
        return;
    }
    let source = cx.sess().source_map();
    let mut values = Vec::new();
    let mut keys = Vec::new();
    let mut bare_names = Vec::new();
    for trait_item in &traits {
        let MetaItemInner::MetaItem(meta) = trait_item else {
            return;
        };
        if !matches!(meta.kind, MetaItemKind::Word) {
            return;
        }
        // A matching final segment is insufficient: custom::Debug can be a
        // procedural derive whose position changes generated code.
        let Some(name) = builtin_derive(&meta.path) else {
            return;
        };
        if meta.path.segments.len() == 1 {
            bare_names.push(name.clone());
        }
        values.push(source.span_to_snippet(meta.span).ok());
        keys.push((name, source.span_to_snippet(meta.path.span).ok()));
    }
    let Some(values): Option<Vec<_>> = values.into_iter().collect() else {
        return;
    };
    let Some(keys): Option<Vec<_>> = keys
        .into_iter()
        .map(|(name, path)| path.map(|path| (name, path)))
        .collect()
    else {
        return;
    };
    if !bare_names.is_empty()
        && imports
            .iter()
            .any(|scope| scope.contains("*") || bare_names.iter().any(|name| scope.contains(name)))
    {
        return;
    }
    let mut indices: Vec<_> = (0..traits.len()).collect();
    indices.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    if indices
        .iter()
        .enumerate()
        .all(|(position, index)| position == *index)
    {
        return;
    }
    let has_comment = source
        .span_to_snippet(attr.span)
        .is_ok_and(|text| text.contains("/*") || text.contains("//"));
    let replacement = (!has_comment).then(|| {
        format!(
            "#[derive({})]",
            indices
                .iter()
                .map(|&i| values[i].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    cx.emit_span_lint(
        UNSORTED_ATTRIBUTES,
        attr.span,
        SortAttributes {
            span: attr.span,
            replacement,
            subject: "derived traits",
        },
    );
}

impl EarlyLintPass for UnsortedAttributes {
    fn check_crate(&mut self, _cx: &EarlyContext<'_>, krate: &rustc_ast::Crate) {
        self.block_imports.clear();
        self.imports.push(module_imports(&krate.items));
    }

    fn check_crate_post(&mut self, _cx: &EarlyContext<'_>, _krate: &rustc_ast::Crate) {
        self.imports.pop();
    }

    fn check_block(&mut self, _cx: &EarlyContext<'_>, block: &Block) {
        let mut imports = HashSet::new();
        for stmt in &block.stmts {
            if let StmtKind::Item(item) = &stmt.kind {
                record_import(item, &mut imports);
            }
        }
        if !imports.is_empty() {
            self.block_imports.push((block.span, imports));
        }
    }

    fn check_item(&mut self, cx: &EarlyContext<'_>, item: &Item) {
        // Items are visited in source order and blocks nest, so trailing
        // blocks that end before this item can no longer contain anything.
        while self
            .block_imports
            .last()
            .is_some_and(|(span, _)| span.hi() < item.span.lo())
        {
            self.block_imports.pop();
        }
        if let ItemKind::Mod(_, _, ModKind::Loaded(items, ..)) = &item.kind {
            self.imports.push(module_imports(items));
        }
        // An unknown attribute can be a macro or a derive helper. Do not
        // rearrange any portion of an item carrying one.
        if item
            .attrs
            .iter()
            .any(|attr| !is_doc(attr) && safe_name(attr).is_none())
        {
            return;
        }
        let moved_span = derive_movement(cx, item).map(|movement| movement.span);
        // Only derive lists consult imports; most items have none.
        let mut imports: Option<Vec<&HashSet<String>>> = None;
        let mut start = 0;
        while start < item.attrs.len() {
            let Some(name) = safe_name(&item.attrs[start]) else {
                start += 1;
                continue;
            };
            if name == "derive" {
                if !moved_span.is_some_and(|span| {
                    item.attrs[start].span.lo() < span.hi()
                        && span.lo() < item.attrs[start].span.hi()
                }) {
                    let imports = imports.get_or_insert_with(|| {
                        self.imports
                            .iter()
                            .chain(self.block_imports.iter().filter_map(|(span, imports)| {
                                (span.lo() <= item.span.lo() && item.span.hi() <= span.hi())
                                    .then_some(imports)
                            }))
                            .collect()
                    });
                    sort_derive(cx, &item.attrs[start], imports);
                }
                start += 1;
                continue;
            }
            if !is_sortable(&name) {
                start += 1;
                continue;
            }
            let mut end = start + 1;
            let mut names = vec![name];
            while end < item.attrs.len() {
                let Some(next) = safe_name(&item.attrs[end]) else {
                    break;
                };
                if !is_sortable(&next) {
                    break;
                }
                names.push(next);
                end += 1;
            }
            let overlaps_move = moved_span.is_some_and(|span| {
                item.attrs[start..end]
                    .iter()
                    .any(|attr| attr.span.lo() < span.hi() && span.lo() < attr.span.hi())
            });
            if !overlaps_move {
                sort_attribute_run(cx, &item.attrs[start..end], &names);
            }
            start = end;
        }
    }

    fn check_item_post(&mut self, _cx: &EarlyContext<'_>, item: &Item) {
        if matches!(item.kind, ItemKind::Mod(_, _, ModKind::Loaded(..))) {
            self.imports.pop();
        }
    }
}

#[cfg(test)]
mod tests {
    const SOURCE: &str = r#"#![allow(dead_code, unused_imports)]

#[repr(C)]
#[cfg(all())]
#[derive(Debug, Clone)]
struct Flagged(u8);

#[cfg(all())]
#[repr(C)]
#[derive(Clone, Debug)]
struct Sorted(u8);

#[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy")]
#[allow(unknown_lints, unsorted_attributes, reason = "CG-007-attribute-order: test suppression")]
#[repr(C)]
#[cfg(all())]
struct Allowed(u8);

#[cfg_attr(any(), must_use)]
#[cfg_attr(any(), deprecated)]
fn conditional() {}

#[inline(never)]
#[cfg_attr(any(), inline(always))]
fn conflicting_inline() {}

#[inline(never)]
#[cfg_attr(all(), inline(always))]
fn conflicting_inline_active() {}

#[derive(custom::Debug, custom::Clone)]
struct CustomDerive;

mod imported_derive {
    use core::fmt::Debug;

    #[derive(Debug, Clone)]
    struct PotentiallyShadowed;
}

mod custom {
    pub use core::fmt::Debug;
}

fn block_named_import() {
    #[derive(Debug, Clone)]
    struct PotentiallyShadowed;
    use crate::custom::Debug;
}

fn block_glob_import() {
    use crate::custom::*;
    {
        #[derive(Debug, Clone)]
        struct PotentiallyShadowed;
    }
}

fn block_without_import() {
    #[derive(Debug, Clone)]
    struct Sortable;
}

#[derive(Debug, /* keep this comment */ Clone)]
struct CommentedDerive;

#[derive(Debug, Clone)]
#[derive(Default)]
struct SeparateDerives;

#[derive(Debug, Clone)]
/// Documentation blocks placement.
#[repr(C)]
struct DeriveBeforeDoc(u8);

#[derive(Clone)]
#[repr(C)]
#[cfg(all())]
struct PlacementAndSort(u8);

#[repr(C)]
/// Keep documentation in source order.
#[cfg(all())]
struct Documented(u8);

#[repr(C)]
#[cfg_attr(any(), custom_attribute)]
#[cfg(all())]
struct UnknownAttribute(u8);

struct Members {
    #[deprecated]
    #[cfg(all())]
    field: u8,
}

enum Variants {
    #[deprecated]
    #[cfg(all())]
    A,
}

impl Members {
    #[inline]
    #[cold]
    fn member(&self) {}
}

fn main() {}
"#;

    const STDERR: &str = r#"error: sort attributes alphabetically
  --> $DIR/unsorted_attributes.rs:3:1
   |
LL | / #[repr(C)]
LL | | #[cfg(all())]
   | |_____________^
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md
   = note: `#[deny(unsorted_attributes)]` on by default
help: use alphabetical order
   |
LL + #[cfg(all())]
LL + #[repr(C)]
   |

error: sort derived traits alphabetically
  --> $DIR/unsorted_attributes.rs:5:1
   |
LL | #[derive(Debug, Clone)]
   | ^^^^^^^^^^^^^^^^^^^^^^^ help: use alphabetical order: `#[derive(Clone, Debug)]`
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md

error: sort attributes alphabetically
  --> $DIR/unsorted_attributes.rs:19:1
   |
LL | / #[cfg_attr(any(), must_use)]
LL | | #[cfg_attr(any(), deprecated)]
   | |______________________________^
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md
help: use alphabetical order
   |
LL + #[cfg_attr(any(), deprecated)]
LL + #[cfg_attr(any(), must_use)]
   |

error: sort derived traits alphabetically
  --> $DIR/unsorted_attributes.rs:60:5
   |
LL |     #[derive(Debug, Clone)]
   |     ^^^^^^^^^^^^^^^^^^^^^^^ help: use alphabetical order: `#[derive(Clone, Debug)]`
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md

error: sort derived traits alphabetically
  --> $DIR/unsorted_attributes.rs:64:1
   |
LL | #[derive(Debug, /* keep this comment */ Clone)]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md

error: sort derived traits alphabetically
  --> $DIR/unsorted_attributes.rs:67:1
   |
LL | #[derive(Debug, Clone)]
   | ^^^^^^^^^^^^^^^^^^^^^^^ help: use alphabetical order: `#[derive(Clone, Debug)]`
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md

error: sort derived traits alphabetically
  --> $DIR/unsorted_attributes.rs:71:1
   |
LL | #[derive(Debug, Clone)]
   | ^^^^^^^^^^^^^^^^^^^^^^^ help: use alphabetical order: `#[derive(Clone, Debug)]`
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md

error: place derive after other outer attributes
  --> $DIR/unsorted_attributes.rs:76:1
   |
LL | / #[derive(Clone)]
LL | | #[repr(C)]
LL | | #[cfg(all())]
   | |_____________^
   |
   = help: see docs/code-guidelines/CG-007-attribute-order.md
   = note: `#[deny(derive_not_last)]` on by default
help: move derive after these attributes
   |
LL + #[repr(C)]
LL + #[cfg(all())]
LL + #[derive(Clone)]
   |

error[E0433]: cannot find `Clone` in `custom`
  --> $DIR/unsorted_attributes.rs:31:33
   |
LL | #[derive(custom::Debug, custom::Clone)]
   |                                 ^^^^^ could not find `Clone` in `custom`

error: aborting due to 9 previous errors

For more information about this error, try `rustc --explain E0433`.
"#;

    #[test]
    fn ui() {
        crate::testing::run_ui("unsorted_attributes", SOURCE, STDERR, &[]);
    }
}
