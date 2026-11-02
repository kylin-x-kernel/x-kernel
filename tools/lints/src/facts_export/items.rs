// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `item` records: every definition of the crate with its span, visibility
//! and the macro that produced it; structs, unions and enums carry their
//! fields with resolved types, statics and consts their type.

use rustc_hir::{
    def::DefKind,
    def_id::{DefId, LOCAL_CRATE, LocalDefId},
};
use rustc_middle::ty::{self, Ty};
use rustc_span::ExpnKind;

use super::{Exporter, json, json::Record};

/// Record kind of a definition, or `None` for kinds the index ignores.
fn item_label(kind: DefKind) -> Option<&'static str> {
    Some(match kind {
        DefKind::Mod => "mod",
        DefKind::Struct => "struct",
        DefKind::Union => "union",
        DefKind::Enum => "enum",
        DefKind::Trait => "trait",
        DefKind::TyAlias => "type",
        DefKind::Fn | DefKind::AssocFn => "fn",
        DefKind::Const { .. } | DefKind::AssocConst { .. } => "const",
        DefKind::Static { .. } => "static",
        DefKind::Macro(_) => "macro",
        DefKind::Impl { .. } => "impl",
        DefKind::AssocTy => "assoc_type",
        DefKind::Closure => "closure",
        _ => return None,
    })
}

impl<'tcx> Exporter<'tcx> {
    pub(super) fn items(&mut self) {
        let tcx = self.tcx;
        // `iter_local_def_id` depends on `analysis`, which is still running
        // here; HIR owners plus closure bodies cover every item kind below.
        let closures: Vec<LocalDefId> = tcx
            .hir_body_owners()
            .filter(|d| tcx.def_kind(*d) == DefKind::Closure)
            .collect();
        let owners: Vec<LocalDefId> = tcx.hir_crate_items(()).definitions().collect();
        for def in owners.into_iter().chain(closures) {
            let kind = tcx.def_kind(def);
            let Some(label) = item_label(kind) else {
                continue;
            };
            self.bump(label);
            let mut line = String::new();
            self.item(&mut line, def, kind, label);
            self.buf.push_str(&line);
        }
    }

    fn item(&mut self, line: &mut String, def: LocalDefId, kind: DefKind, label: &str) {
        let tcx = self.tcx;
        let def_id = def.to_def_id();
        let span = tcx.def_span(def);
        let place = self.place(span);
        let expn = tcx.expn_that_defined(def_id).expn_data();
        let defined_by = match expn.kind {
            ExpnKind::Macro(k, n) => Some(ExpnKind::Macro(k, n).descr()),
            _ => None,
        };
        let mac = defined_by.or(place.mac.clone());
        // Generated: the item's name (or, without one, its span) is macro
        // output rather than source text that a macro passed through.
        let name_span = tcx.def_ident_span(def_id).unwrap_or(span);
        let generated = name_span.from_expansion();
        if generated {
            self.bump("items_generated");
        }
        let full = tcx.hir_span_with_body(tcx.local_def_id_to_hir_id(def));
        let full_end = tcx
            .sess
            .source_map()
            .lookup_char_pos(full.source_callsite().hi())
            .line as u64;
        let vis = if matches!(kind, DefKind::Impl { .. } | DefKind::Closure) {
            None
        } else {
            let module = tcx.parent_module_from_def_id(def).to_def_id();
            Some(match tcx.visibility(def_id) {
                ty::Visibility::Public => "pub",
                ty::Visibility::Restricted(m) if m == module => "priv",
                ty::Visibility::Restricted(m) if m == LOCAL_CRATE.as_def_id() => "crate",
                ty::Visibility::Restricted(_) => "restricted",
            })
        };
        let name = tcx.opt_item_name(def_id).map(|s| s.to_string());
        let mut record = Record::new(line, "item")
            .str("key", &self.key(def_id))
            .str("path", &self.pretty(def_id))
            .str("kind", label)
            .str("name", name.as_deref().unwrap_or(""));
        record = Self::put_place(record, &place)
            .num("end_line", full_end.max(place.end_line))
            .opt("vis", vis)
            .opt("defined_by", mac.as_deref())
            .flag("gen", generated);
        if let Some(parent) = tcx.opt_parent(def_id) {
            record = record.str("parent", &self.key(parent));
        }
        record = match kind {
            DefKind::Impl { of_trait } => self.impl_fields(record, def_id, of_trait),
            DefKind::Fn | DefKind::AssocFn => self.fn_fields(record, def_id, kind),
            DefKind::Closure => record.str("root", &self.key(tcx.typeck_root_def_id(def_id))),
            DefKind::Struct | DefKind::Union => {
                let adt = tcx.adt_def(def_id);
                record.raw("fields", &self.fields(adt.non_enum_variant()))
            }
            DefKind::Enum => record.raw("variants", &self.variants(def_id)),
            DefKind::Static { .. } | DefKind::Const { .. } | DefKind::AssocConst { .. } => {
                let ty = tcx.type_of(def_id).instantiate_identity();
                self.typed(record, "ty", "ty_adts", ty)
            }
            _ => record,
        };
        record.end();
    }

    /// Writes a type and the ADTs and `dyn` traits it mentions (by key), so
    /// that "who holds a `T`" is a key lookup rather than a string search.
    fn typed<'a>(
        &self,
        record: Record<'a>,
        ty_key: &str,
        adts_key: &str,
        ty: Ty<'tcx>,
    ) -> Record<'a> {
        let mentioned = self.mentioned_types(ty);
        record
            .str(ty_key, &ty::print::with_no_trimmed_paths!(ty.to_string()))
            .raw(
                adts_key,
                &json::str_array(mentioned.iter().map(String::as_str)),
            )
    }

    /// Keys of the ADTs and `dyn` principal traits in `ty`, outermost first.
    fn mentioned_types(&self, ty: Ty<'tcx>) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for arg in ty.walk() {
            let Some(inner) = arg.as_type() else {
                continue;
            };
            let def = match inner.kind() {
                ty::Adt(adt, _) => Some(adt.did()),
                ty::Dynamic(preds, ..) => preds.principal_def_id(),
                ty::Foreign(def) => Some(*def),
                _ => None,
            };
            if let Some(def) = def {
                let key = self.key(def);
                if !keys.contains(&key) {
                    keys.push(key);
                }
            }
        }
        keys
    }

    /// `[{name, ty, adts, line}]` for the fields of one variant.
    fn fields(&self, variant: &ty::VariantDef) -> String {
        let tcx = self.tcx;
        json::array(variant.fields.iter().map(|field| {
            let ty = tcx.type_of(field.did).instantiate_identity();
            let line = self.def_line(field.did);
            let mut out = String::new();
            let record = Record::object(&mut out)
                .str("name", field.name.as_str())
                .str("key", &self.key(field.did));
            self.typed(record, "ty", "adts", ty).num("line", line).end();
            out
        }))
    }

    /// `[{name, key, line, fields}]` for the variants of an enum.
    fn variants(&self, def_id: DefId) -> String {
        let adt = self.tcx.adt_def(def_id);
        json::array(adt.variants().iter().map(|variant| {
            let mut out = String::new();
            Record::object(&mut out)
                .str("name", variant.name.as_str())
                .str("key", &self.key(variant.def_id))
                .num("line", self.def_line(variant.def_id))
                .raw("fields", &self.fields(variant))
                .end();
            out
        }))
    }

    fn def_line(&self, def_id: DefId) -> u64 {
        let span = self.tcx.def_span(def_id).source_callsite();
        self.tcx.sess.source_map().lookup_char_pos(span.lo()).line as u64
    }

    fn impl_fields<'a>(
        &mut self,
        mut record: Record<'a>,
        def_id: DefId,
        of_trait: bool,
    ) -> Record<'a> {
        let tcx = self.tcx;
        let self_ty = tcx.type_of(def_id).instantiate_identity();
        record = record
            .str(
                "self_ty",
                &ty::print::with_no_trimmed_paths!(self_ty.to_string()),
            )
            .str("owner", &self.type_short(self_ty));
        if !of_trait {
            self.bump("inherent_impls");
            return record;
        }
        let trait_id = tcx.impl_trait_ref(def_id).instantiate_identity().def_id;
        self.bump("trait_impls");
        record
            .str("trait", &self.key(trait_id))
            .str("trait_name", tcx.item_name(trait_id).as_str())
            .flag(
                "negative",
                tcx.impl_polarity(def_id) == ty::ImplPolarity::Negative,
            )
    }

    fn fn_fields<'a>(&self, mut record: Record<'a>, def_id: DefId, kind: DefKind) -> Record<'a> {
        let tcx = self.tcx;
        let sig = tcx.fn_sig(def_id).skip_binder();
        record = record.flag("unsafe", sig.safety().is_unsafe()).flag(
            "generic",
            tcx.generics_of(def_id).requires_monomorphization(tcx),
        );
        if let Some(sym) = tcx.codegen_fn_attrs(def_id).symbol_name {
            record = record.str("symbol", sym.as_str());
        }
        if kind != DefKind::AssocFn {
            return record;
        }
        let Some(parent) = tcx.opt_parent(def_id) else {
            return record;
        };
        match tcx.def_kind(parent) {
            DefKind::Impl { of_trait } => {
                let self_ty = tcx.type_of(parent).instantiate_identity();
                record = record.str("owner", &self.type_short(self_ty));
                if of_trait {
                    let trait_id = tcx.impl_trait_ref(parent).instantiate_identity().def_id;
                    record = record.str("trait_name", tcx.item_name(trait_id).as_str());
                }
                record
            }
            DefKind::Trait => record.str("trait_name", tcx.item_name(parent).as_str()),
            _ => record,
        }
    }
}
