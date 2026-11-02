// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Where a value in a MIR body comes from, following locals that are
//! assigned once: links function values to the calls, parameters, fields and
//! closure captures they flow through, which narrows fn-pointer and generic
//! `Fn*` calls.

use std::collections::HashMap;

use rustc_abi::FIRST_VARIANT;
use rustc_hir::{
    Node,
    def::DefKind,
    def_id::{DefId, LocalDefId},
};
use rustc_middle::{
    hir::place::ProjectionKind,
    mir::{self, AggregateKind, Local, Operand, PlaceRef, PlaceTy, ProjectionElem, Rvalue},
    ty::{self, TyCtxt},
};
use rustc_span::Symbol;

/// Longest chain of single assignments followed.
const MAX_DEPTH: usize = 8;

/// The source of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The body's parameter at this index (0-based).
    Param(usize),
    /// Read from this field.
    Field(DefId),
    /// This function item.
    Fn(DefId),
    /// This closure.
    Closure(DefId),
    /// A variable the closure body captured, and where that variable comes
    /// from in the enclosing body.
    Captured {
        name: Symbol,
        source: Captured,
    },
    Unknown,
}

/// Source of a captured variable in the body enclosing a closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Captured {
    /// Parameter `index` (0-based) of the enclosing fn `owner`.
    Param {
        owner: DefId,
        index: usize,
    },
    /// A field read by a precise capture (`self.handler`).
    Field(DefId),
    /// A function item or closure: the captured value itself, or the
    /// initialiser of the captured `let` binding.
    Value(DefId),
    Unknown,
}

impl Origin {
    /// The function or closure this value is known to be.
    pub(super) fn value(self) -> Option<DefId> {
        match self {
            Self::Fn(def)
            | Self::Closure(def)
            | Self::Captured {
                source: Captured::Value(def),
                ..
            } => Some(def),
            _ => None,
        }
    }
}

/// Single-assignment view of one body.
pub(super) struct Flow<'b, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'b mir::Body<'tcx>,
    /// The body's owner when it is a closure: its first local is the
    /// captured environment.
    closure: Option<LocalDefId>,
    /// The rvalue of locals assigned exactly once by a statement; `None` for
    /// locals assigned more than once or written by a call.
    assigned: HashMap<Local, Option<&'b Rvalue<'tcx>>>,
}

impl<'b, 'tcx> Flow<'b, 'tcx> {
    pub(super) fn new(tcx: TyCtxt<'tcx>, body: &'b mir::Body<'tcx>, def: LocalDefId) -> Self {
        let mut assigned: HashMap<Local, Option<&'b Rvalue<'tcx>>> = HashMap::new();
        for block in body.basic_blocks.iter() {
            for stmt in &block.statements {
                let mir::StatementKind::Assign(assign) = &stmt.kind else {
                    continue;
                };
                let (place, rvalue) = &**assign;
                if let Some(local) = place.as_local() {
                    assigned
                        .entry(local)
                        .and_modify(|slot| *slot = None)
                        .or_insert(Some(rvalue));
                }
            }
            if let Some(mir::Terminator {
                kind: mir::TerminatorKind::Call { destination, .. },
                ..
            }) = &block.terminator
            {
                assigned.insert(destination.local, None);
            }
        }
        let closure = tcx.is_closure_like(def.to_def_id()).then_some(def);
        Self {
            tcx,
            body,
            closure,
            assigned,
        }
    }

    pub(super) fn operand(&self, operand: &Operand<'tcx>) -> Origin {
        self.operand_at(operand, 0)
    }

    fn operand_at(&self, operand: &Operand<'tcx>, depth: usize) -> Origin {
        match operand {
            Operand::Constant(constant) => match constant.const_.ty().kind() {
                ty::FnDef(def, _) => Origin::Fn(*def),
                ty::Closure(def, _) => Origin::Closure(*def),
                _ => Origin::Unknown,
            },
            Operand::Copy(place) | Operand::Move(place) => self.place_at(place.as_ref(), depth),
            Operand::RuntimeChecks(_) => Origin::Unknown,
        }
    }

    fn place_at(&self, place: PlaceRef<'tcx>, depth: usize) -> Origin {
        if depth > MAX_DEPTH {
            return Origin::Unknown;
        }
        if let Some(index) = self.capture_index(place) {
            return self.captured(index);
        }
        if let Some(field) = self.field_of(place) {
            return Origin::Field(field);
        }
        if place
            .projection
            .iter()
            .any(|elem| !matches!(elem, ProjectionElem::Deref))
        {
            return Origin::Unknown;
        }
        let index = place.local.as_usize();
        if (1..=self.body.arg_count).contains(&index) {
            return self.param(index).map_or(Origin::Unknown, Origin::Param);
        }
        let Some(Some(rvalue)) = self.assigned.get(&place.local) else {
            return Origin::Unknown;
        };
        match rvalue {
            Rvalue::Use(operand) | Rvalue::Cast(_, operand, _) => {
                self.operand_at(operand, depth + 1)
            }
            Rvalue::Ref(_, _, inner) | Rvalue::RawPtr(_, inner) => {
                self.place_at(inner.as_ref(), depth + 1)
            }
            Rvalue::Aggregate(kind, _) => match **kind {
                AggregateKind::Closure(def, _) => Origin::Closure(def),
                _ => Origin::Unknown,
            },
            _ => Origin::Unknown,
        }
    }

    /// The declared parameter (0-based) held by argument local `_index`.
    ///
    /// In a closure body `_1` is the captured environment and the declared
    /// parameters start at `_2`; a coroutine's other argument is its resume
    /// value, not a declared parameter.
    fn param(&self, index: usize) -> Option<usize> {
        match self.closure {
            None => index.checked_sub(1),
            Some(def) if self.tcx.is_coroutine(def.to_def_id()) => None,
            Some(_) => index.checked_sub(2),
        }
    }

    /// The capture that `place` reads: `(*_1).i` or `_1.i` in a closure body,
    /// possibly dereferenced further (a by-reference capture).
    fn capture_index(&self, place: PlaceRef<'tcx>) -> Option<usize> {
        self.closure?;
        if place.local.as_usize() != 1 {
            return None;
        }
        let mut elems = place
            .projection
            .iter()
            .skip_while(|elem| matches!(elem, ProjectionElem::Deref));
        let ProjectionElem::Field(index, _) = elems.next()? else {
            return None;
        };
        let env = self.body.local_decls[place.local].ty.peel_refs();
        if !matches!(
            env.kind(),
            ty::Closure(..) | ty::CoroutineClosure(..) | ty::Coroutine(..)
        ) {
            return None;
        }
        elems
            .all(|elem| matches!(elem, ProjectionElem::Deref))
            .then_some(index.as_usize())
    }

    /// Resolves capture `index` of this closure in the enclosing body, from
    /// the capture analysis and the enclosing body's HIR.
    fn captured(&self, index: usize) -> Origin {
        let tcx = self.tcx;
        let Some(closure) = self.closure else {
            return Origin::Unknown;
        };
        let Some(capture) = tcx.closure_captures(closure).get(index) else {
            return Origin::Unknown;
        };
        let name = capture.to_symbol();
        let place = &capture.place;
        let source = if let Some(last) = place.projections.last() {
            match last.kind {
                ProjectionKind::Field(field, variant) => {
                    match place
                        .ty_before_projection(place.projections.len() - 1)
                        .kind()
                    {
                        ty::Adt(adt, _) => Captured::Field(adt.variant(variant).fields[field].did),
                        _ => Captured::Unknown,
                    }
                }
                _ => Captured::Unknown,
            }
        } else {
            match place.ty().kind() {
                ty::FnDef(def, _) | ty::Closure(def, _) => Captured::Value(*def),
                _ => self.enclosing_binding(closure, capture.get_root_variable()),
            }
        };
        Origin::Captured { name, source }
    }

    /// Where the variable `var` of the body enclosing `closure` comes from:
    /// a parameter of an enclosing fn, or a `let` initialised with a function
    /// item or closure.
    fn enclosing_binding(&self, closure: LocalDefId, var: rustc_hir::HirId) -> Captured {
        let tcx = self.tcx;
        let owner = tcx.local_parent(closure);
        // Closure parameters are numbered after the captured environment in
        // MIR, so only fn parameters map onto `passed_to` / `arg`.
        if matches!(tcx.def_kind(owner), DefKind::Fn | DefKind::AssocFn)
            && let Some(body) = tcx.hir_maybe_body_owned_by(owner)
            && let Some(index) = body.params.iter().position(|param| param.pat.hir_id == var)
        {
            return Captured::Param {
                owner: owner.to_def_id(),
                index,
            };
        }
        if let Node::LetStmt(local) = tcx.parent_hir_node(var)
            && let Some(init) = local.init
        {
            let ty = tcx.typeck(owner).expr_ty(init);
            if let ty::FnDef(def, _) | ty::Closure(def, _) = ty.kind() {
                return Captured::Value(*def);
            }
        }
        Captured::Unknown
    }

    /// The ADT field that `place` ends in, if its last field projection is
    /// on a struct, union or enum variant.
    pub(super) fn field_of(&self, place: PlaceRef<'tcx>) -> Option<DefId> {
        let mut place_ty = PlaceTy::from_ty(self.body.local_decls[place.local].ty);
        let mut field = None;
        for elem in place.projection {
            if let ProjectionElem::Field(index, _) = elem {
                field = match place_ty.ty.kind() {
                    ty::Adt(adt, _) => {
                        let variant = place_ty.variant_index.unwrap_or(FIRST_VARIANT);
                        Some(adt.variant(variant).fields[*index].did)
                    }
                    _ => None,
                };
            }
            place_ty = place_ty.projection_ty(self.tcx, *elem);
        }
        field
    }
}
